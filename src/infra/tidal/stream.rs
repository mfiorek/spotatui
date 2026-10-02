//! The track download: a BTS manifest's CDN URL, played while it downloads.
//!
//! A `stream-download` [`HttpStream`] over the URL fills the session's
//! tempfile through [`crate::infra::progressive`]; a seek past the downloaded
//! part becomes a `Range` request. This is the one place in the Tidal module
//! that speaks `stream-download`'s own `reqwest` (which can be a different
//! version from the crate's), and its client has no total timeout: a request
//! timeout would cut every track off mid-download.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use stream_download::http::{reqwest, HttpStream, HttpStreamError};
use stream_download::source::SourceStream;
use stream_download::Settings;
use tempfile::NamedTempFile;

use crate::infra::progressive::{self, Completion, TrackReader};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Time without a byte before a read fails; bounds the header wait too.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes downloaded before the first read returns: about 6 s of AAC 320.
const PREFETCH_BYTES: u64 = 256 * 1024;
/// Time without a chunk before `stream-download` reconnects at the current
/// position. Above [`READ_TIMEOUT`], so a stalled read fails there first.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// A track whose download has started.
pub struct OpenedTrack {
  pub reader: TrackReader,
  pub completion: Completion,
  /// The file size, when the CDN sent one; the decoder seeks only with it.
  pub byte_len: Option<u64>,
}

/// The process-wide client for track downloads.
fn stream_client() -> reqwest::Client {
  static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
  CLIENT
    .get_or_init(|| {
      reqwest::Client::builder()
        .user_agent(super::USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()
        .unwrap_or_default()
    })
    .clone()
}

/// Start downloading `url` into `file`.
pub async fn open(url: &str, file: &NamedTempFile) -> Result<OpenedTrack> {
  open_with(stream_client(), url, file, PREFETCH_BYTES).await
}

async fn open_with(
  client: reqwest::Client,
  url: &str,
  file: &NamedTempFile,
  prefetch_bytes: u64,
) -> Result<OpenedTrack> {
  let parsed: reqwest::Url = url.parse().context("the stream URL")?;
  let stream = HttpStream::new(client, parsed).await.map_err(describe)?;
  let byte_len = stream.content_length();
  let settings = Settings::default()
    .prefetch_bytes(prefetch_bytes)
    .retry_timeout(STALL_TIMEOUT);
  let (reader, completion) = progressive::open(stream, file, settings).await?;
  Ok(OpenedTrack {
    reader,
    completion,
    byte_len,
  })
}

/// The error without the URL: its query carries the CDN token.
fn describe(err: HttpStreamError<reqwest::Client>) -> anyhow::Error {
  match err {
    HttpStreamError::FetchFailure(e) => anyhow!("track download: {}", e.without_url()),
    HttpStreamError::ResponseFailure(e) => {
      anyhow!("track download: HTTP {}", e.response().status())
    }
  }
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Seek, SeekFrom};

  use super::super::test_server::serve_file;
  use super::*;

  /// Large enough that the first response is still running at the seek.
  const SIZE: usize = 1 << 20;
  const PREFETCH: u64 = 64 * 1024;

  fn body() -> Vec<u8> {
    (0..SIZE).map(|i| (i % 251) as u8).collect()
  }

  async fn wait_for(completion: &Completion) -> bool {
    for _ in 0..500 {
      if completion.is_complete() {
        return true;
      }
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
  }

  #[tokio::test]
  async fn the_reader_returns_the_whole_file_and_fills_the_tempfile() {
    let expected = body();
    let (url, _server) = serve_file(expected.clone(), None).await;
    let tmp = NamedTempFile::new().unwrap();
    let opened = open_with(reqwest::Client::new(), &url, &tmp, PREFETCH)
      .await
      .unwrap();
    assert_eq!(opened.byte_len, Some(SIZE as u64));
    let mut reader = opened.reader;
    let bytes = tokio::task::spawn_blocking(move || {
      let mut out = Vec::new();
      reader.read_to_end(&mut out).map(|_| (out, reader))
    })
    .await
    .unwrap()
    .unwrap()
    .0;
    assert_eq!(bytes, expected);
    assert!(wait_for(&opened.completion).await);
    assert_eq!(std::fs::read(tmp.path()).unwrap(), expected);
  }

  /// The shape of an MP4 with its `moov` box at the end: the probe reads the
  /// tail first, then decoding starts from the top.
  #[tokio::test]
  async fn a_read_of_the_tail_then_the_top_yields_exact_bytes_and_a_full_file() {
    let expected = body();
    let (url, server) = serve_file(expected.clone(), None).await;
    let tmp = NamedTempFile::new().unwrap();
    let opened = open_with(reqwest::Client::new(), &url, &tmp, PREFETCH)
      .await
      .unwrap();
    let mut reader = opened.reader;
    let (tail, all, reader) = tokio::task::spawn_blocking(move || {
      reader.seek(SeekFrom::End(-1000)).unwrap();
      let mut tail = Vec::new();
      reader.read_to_end(&mut tail).unwrap();
      reader.seek(SeekFrom::Start(0)).unwrap();
      let mut all = Vec::new();
      reader.read_to_end(&mut all).unwrap();
      (tail, all, reader)
    })
    .await
    .unwrap();
    assert_eq!(tail, expected[SIZE - 1000..]);
    assert_eq!(all, expected);
    assert!(wait_for(&opened.completion).await);
    drop(reader);
    assert_eq!(std::fs::read(tmp.path()).unwrap(), expected);
    let ranges = server.ranges();
    assert!(
      ranges
        .iter()
        .flatten()
        .any(|r| r.starts_with("bytes=1047576-")),
      "{ranges:?}"
    );
  }

  #[tokio::test]
  async fn a_reader_dropped_after_a_forward_seek_leaves_the_file_incomplete() {
    let expected = body();
    // Every request after the tail's hangs: the gap is never filled.
    let (url, _server) = serve_file(expected.clone(), Some(2)).await;
    let tmp = NamedTempFile::new().unwrap();
    let opened = open_with(reqwest::Client::new(), &url, &tmp, PREFETCH)
      .await
      .unwrap();
    let mut reader = opened.reader;
    let tail = tokio::task::spawn_blocking(move || {
      reader.seek(SeekFrom::End(-1000)).unwrap();
      let mut tail = vec![0; 1000];
      reader.read_exact(&mut tail).unwrap();
      tail
    })
    .await
    .unwrap();
    assert_eq!(tail, expected[SIZE - 1000..]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!opened.completion.is_complete());
  }

  #[tokio::test]
  async fn a_refused_download_names_the_status_but_not_the_url() {
    let (base, _server) =
      super::super::test_server::serve(vec![super::super::test_server::Reply::new(
        "403 Forbidden",
        "{}",
      )])
      .await;
    let tmp = NamedTempFile::new().unwrap();
    let url = format!("{base}/track.m4a?token=secret");
    let err = open_with(reqwest::Client::new(), &url, &tmp, PREFETCH)
      .await
      .err()
      .unwrap();
    let message = format!("{err:#}");
    assert!(message.contains("403"), "{message}");
    assert!(!message.contains("secret"), "{message}");
  }
}
