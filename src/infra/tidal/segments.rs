//! The hi-res download: a DASH manifest's segments, played while they arrive.
//!
//! The init segment gives the FLAC header ([`dash::parse_init`]). Every media
//! segment is then measured with a small `Range` request (its `moof` and the
//! `mdat` header), so the rebuilt file's size and each segment's place in it
//! are known before any audio. The shared [`SegmentStream`] then yields the
//! header and each segment's `mdat` payload into the session's tempfile
//! through [`crate::infra::progressive`]; a seek restarts it at the segment
//! that holds the target byte. Errors never print a URL: its query carries
//! the CDN token.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, Response, StatusCode};
use stream_download::Settings;
use tempfile::NamedTempFile;

use super::dash::{self, DashManifest, FlacFormat, PayloadLayout};
use crate::infra::progressive::{
  self, ChunkFuture, Completion, FetchChunk, SegmentStream, TrackReader,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A whole segment (about 4 s of audio, under 1 MB at 24/192) per request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// Segments measured at once before playback starts.
const PARALLEL_PROBES: usize = 8;
/// The first read of each segment: the live `moof` is about 300 bytes.
const PROBE_BYTES: u64 = 1024;
/// Reads of one segment's header before giving up on finding its `mdat`.
const MAX_PROBES: usize = 3;
/// Bytes downloaded before the first read returns: about 2 s of 24/48.
const PREFETCH_BYTES: u64 = 512 * 1024;
/// Time without a chunk before `stream-download` asks for a reconnect. Above
/// [`REQUEST_TIMEOUT`], so a stalled segment fails there first and counts as
/// an attempt.
const STALL_TIMEOUT: Duration = Duration::from_secs(90);

/// A hi-res track whose download has started.
pub struct OpenedDash {
  pub reader: TrackReader,
  pub completion: Completion,
  /// The size of the rebuilt FLAC file.
  pub byte_len: u64,
  pub format: FlacFormat,
}

/// The process-wide client for segment downloads.
fn segment_client() -> Client {
  static CLIENT: OnceLock<Client> = OnceLock::new();
  CLIENT
    .get_or_init(|| {
      Client::builder()
        .user_agent(super::USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_default()
    })
    .clone()
}

/// Measure the manifest's segments and start downloading them into `file`.
pub async fn open(manifest: &DashManifest, file: &NamedTempFile) -> Result<OpenedDash> {
  open_with(segment_client(), manifest, file, PREFETCH_BYTES).await
}

async fn open_with(
  http: Client,
  manifest: &DashManifest,
  file: &NamedTempFile,
  prefetch_bytes: u64,
) -> Result<OpenedDash> {
  let init = get(&http, &manifest.init_url, None)
    .await
    .context("init segment")?
    .bytes()
    .await
    .map_err(|e| anyhow!("init segment body: {}", e.without_url()))?;
  let init = dash::parse_init(&init).context("init segment")?;

  // Owned futures: borrowed ones trip the compiler's `Send` check in the
  // fetch task that awaits this.
  let probes = manifest
    .segment_urls
    .iter()
    .cloned()
    .enumerate()
    .map(|(i, url)| {
      let http = http.clone();
      async move { payload_len(&http, &url, i + 1).await }
    });
  let lengths: Vec<u64> = futures::stream::iter(probes)
    .buffered(PARALLEL_PROBES)
    .try_collect()
    .await?;

  let urls = Arc::new(manifest.segment_urls.clone());
  let expected = Arc::new(lengths.clone());
  let fetch: FetchChunk = Arc::new(move |number: usize| -> ChunkFuture {
    let http = http.clone();
    let urls = Arc::clone(&urls);
    let expected = Arc::clone(&expected);
    Box::pin(
      async move { fetch_payload(&http, &urls[number - 1], number, expected[number - 1]).await },
    )
  });
  let stream = SegmentStream::new(Bytes::from(init.header), lengths, fetch);
  let byte_len = stream.total_bytes();
  let settings = Settings::default()
    .prefetch_bytes(prefetch_bytes)
    .retry_timeout(STALL_TIMEOUT);
  let (reader, completion) = progressive::open(stream, file, settings).await?;
  Ok(OpenedDash {
    reader,
    completion,
    byte_len,
    format: init.format,
  })
}

/// Send a GET (with `range`, if any) and refuse anything but a success.
async fn get(http: &Client, url: &str, range: Option<String>) -> Result<Response> {
  let mut request = http.get(url);
  if let Some(range) = range {
    request = request.header(RANGE, range);
  }
  let response = request
    .send()
    .await
    .map_err(|e| anyhow!("request: {}", e.without_url()))?;
  let status = response.status();
  if !status.is_success() {
    return Err(anyhow!("HTTP {status}"));
  }
  Ok(response)
}

/// The size of segment `number`'s FLAC frames, from the first bytes of it.
async fn payload_len(http: &Client, url: &str, number: usize) -> Result<u64> {
  let mut want = PROBE_BYTES;
  for _ in 0..MAX_PROBES {
    let mut response = get(http, url, Some(format!("bytes=0-{}", want - 1)))
      .await
      .with_context(|| format!("segment {number} probe"))?;
    let total = if response.status() == StatusCode::PARTIAL_CONTENT {
      content_range_total(&response)
    } else {
      // A server that ignores `Range` sends the whole segment: read only
      // the start, then drop the connection.
      response.content_length()
    };
    let mut prefix = Vec::new();
    while (prefix.len() as u64) < want {
      match response
        .chunk()
        .await
        .map_err(|e| anyhow!("segment {number} probe body: {}", e.without_url()))?
      {
        Some(chunk) => prefix.extend_from_slice(&chunk),
        None => break,
      }
    }
    match dash::media_payload(&prefix, total).with_context(|| format!("segment {number}"))? {
      PayloadLayout::Found { len, .. } => return Ok(len),
      PayloadLayout::NeedBytes(more) => want = more,
    }
  }
  Err(anyhow!(
    "segment {number}: no mdat box in its first {want} bytes"
  ))
}

/// The full size from a `Content-Range: bytes a-b/total` header.
fn content_range_total(response: &Response) -> Option<u64> {
  let value = response.headers().get(CONTENT_RANGE)?.to_str().ok()?;
  value.rsplit_once('/')?.1.trim().parse().ok()
}

/// Fetch segment `number` and return its FLAC frames, which must be the
/// `expected` size measured up front: every later byte offset depends on it.
async fn fetch_payload(http: &Client, url: &str, number: usize, expected: u64) -> Result<Bytes> {
  let body = get(http, url, None)
    .await
    .with_context(|| format!("segment {number}"))?
    .bytes()
    .await
    .map_err(|e| anyhow!("segment {number} body: {}", e.without_url()))?;
  match dash::media_payload(&body, Some(body.len() as u64))
    .with_context(|| format!("segment {number}"))?
  {
    PayloadLayout::Found { offset, len } if len == expected => {
      Ok(body.slice(offset as usize..(offset + len) as usize))
    }
    PayloadLayout::Found { len, .. } => Err(anyhow!(
      "segment {number} holds {len} bytes of audio, {expected} were measured"
    )),
    PayloadLayout::NeedBytes(_) => Err(anyhow!("segment {number} is truncated")),
  }
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Seek, SeekFrom};

  use super::super::dash::fixtures::*;
  use super::super::test_server::serve_files;
  use super::*;

  const PREFETCH: u64 = 1024;

  /// A three-segment track: the served files, the manifest's paths and the
  /// rebuilt FLAC file. The second `moof` is larger than one probe.
  fn track() -> (Vec<(String, Vec<u8>)>, Vec<u8>) {
    let frames: Vec<Vec<u8>> = (0..3u8)
      .map(|s| (0..20_000u32).map(|i| (i as u8) ^ (s * 37)).collect())
      .collect();
    let moofs = [280, 3000, 300];
    let mut files = vec![("/t/0.mp4".to_string(), init_segment(&streaminfo_block()))];
    for (i, (frames, moof)) in frames.iter().zip(moofs).enumerate() {
      files.push((format!("/t/{}.mp4", i + 1), media_segment(moof, frames)));
    }
    let mut expected = b"fLaC".to_vec();
    expected.extend(streaminfo_block());
    expected[4] |= 0x80;
    for f in &frames {
      expected.extend_from_slice(f);
    }
    (files, expected)
  }

  fn manifest(base: &str, segments: usize) -> DashManifest {
    DashManifest {
      init_url: format!("{base}/t/0.mp4?token=secret"),
      segment_urls: (1..=segments)
        .map(|n| format!("{base}/t/{n}.mp4?token=secret"))
        .collect(),
    }
  }

  async fn read_all(opened: OpenedDash) -> Vec<u8> {
    let mut reader = opened.reader;
    tokio::task::spawn_blocking(move || {
      let mut out = Vec::new();
      reader.read_to_end(&mut out).unwrap();
      out
    })
    .await
    .unwrap()
  }

  #[tokio::test]
  async fn the_reader_returns_the_rebuilt_flac_file() {
    let (files, expected) = track();
    let (base, server) = serve_files(files, None, true).await;
    let tmp = NamedTempFile::new().unwrap();
    let opened = open_with(Client::new(), &manifest(&base, 3), &tmp, PREFETCH)
      .await
      .unwrap();
    assert_eq!(opened.byte_len, expected.len() as u64);
    assert_eq!(opened.format.label(), "FLAC 24/48");
    assert_eq!(read_all(opened).await, expected);
    assert_eq!(std::fs::read(tmp.path()).unwrap(), expected);
    // Each segment is measured with a small range; the larger moof asks again.
    let probes: Vec<_> = server
      .requests()
      .into_iter()
      .filter(|(_, range)| range.is_some())
      .collect();
    assert_eq!(probes.len(), 4, "{probes:?}");
    assert!(probes.contains(&("/t/2.mp4".to_string(), Some("bytes=0-3015".to_string()))));
  }

  #[tokio::test]
  async fn a_server_that_ignores_range_still_works() {
    let (files, expected) = track();
    let (base, _server) = serve_files(files, None, false).await;
    let tmp = NamedTempFile::new().unwrap();
    let opened = open_with(Client::new(), &manifest(&base, 3), &tmp, PREFETCH)
      .await
      .unwrap();
    assert_eq!(read_all(opened).await, expected);
  }

  #[tokio::test]
  async fn the_reader_seeks_into_a_later_segment() {
    let (files, expected) = track();
    let (base, _server) = serve_files(files, None, true).await;
    let tmp = NamedTempFile::new().unwrap();
    let opened = open_with(Client::new(), &manifest(&base, 3), &tmp, PREFETCH)
      .await
      .unwrap();
    let mut reader = opened.reader;
    let at = expected.len() - 25_000;
    let (tail, all) = tokio::task::spawn_blocking(move || {
      reader.seek(SeekFrom::Start(at as u64)).unwrap();
      let mut tail = Vec::new();
      reader.read_to_end(&mut tail).unwrap();
      reader.seek(SeekFrom::Start(0)).unwrap();
      let mut all = Vec::new();
      reader.read_to_end(&mut all).unwrap();
      (tail, all)
    })
    .await
    .unwrap();
    assert_eq!(tail, expected[at..]);
    assert_eq!(all, expected);
  }

  #[tokio::test]
  async fn a_missing_segment_fails_the_open_without_naming_the_url() {
    let (files, _) = track();
    let (base, _server) = serve_files(files, None, true).await;
    let tmp = NamedTempFile::new().unwrap();
    let err = open_with(Client::new(), &manifest(&base, 4), &tmp, PREFETCH)
      .await
      .err()
      .unwrap();
    let message = format!("{err:#}");
    assert!(
      message.contains("segment 4") && message.contains("404"),
      "{message}"
    );
    assert!(!message.contains("secret"), "{message}");
  }

  #[tokio::test]
  async fn an_init_segment_that_is_not_flac_fails_the_open() {
    let (mut files, _) = track();
    files[0].1 = mp4_box(b"ftyp", b"iso6");
    let (base, _server) = serve_files(files, None, true).await;
    let tmp = NamedTempFile::new().unwrap();
    assert!(
      open_with(Client::new(), &manifest(&base, 3), &tmp, PREFETCH)
        .await
        .is_err()
    );
  }

  #[tokio::test]
  async fn a_segment_whose_size_changed_is_refused() {
    let (files, _) = track();
    let (base, _server) = serve_files(files, None, true).await;
    let http = Client::new();
    let url = format!("{base}/t/1.mp4");
    assert_eq!(payload_len(&http, &url, 1).await.unwrap(), 20_000);
    assert_eq!(
      fetch_payload(&http, &url, 1, 20_000).await.unwrap().len(),
      20_000
    );
    let err = fetch_payload(&http, &url, 1, 19_999).await.unwrap_err();
    assert!(err.to_string().contains("measured"), "{err:#}");
  }
}
