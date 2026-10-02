//! Progressive delivery: the track plays while it downloads.
//!
//! [`segment_stream`] builds the shared [`SegmentStream`] over the segment
//! transport: the codec header, then each decrypted audio segment in order,
//! restarting at any byte offset when the decoder seeks past the downloaded
//! part. [`crate::infra::progressive`] writes the bytes into the session's
//! tempfile, blocks the decoder's reads until they exist, and stops the
//! download when the reader is dropped. The session keeps the
//! `NamedTempFile`, so the finished file outlives the reader (repeat-one).

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use reqwest::Client;
use stream_download::Settings;
use tempfile::NamedTempFile;

use super::cmaf::InitSegment;
use super::download::fetch_audio_segment;

/// Bytes downloaded before the first read returns: a cushion against jitter
/// that costs well under a second on a normal connection.
const PREFETCH_BYTES: u64 = 512 * 1024;
/// Time without a chunk before `stream-download` asks for a reconnect. Above
/// the HTTP client's request timeout, so a stalled segment fails there first
/// and counts as an attempt.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

pub use crate::infra::progressive::{SegmentStream, TrackReader};

/// Start the download into `file` and return the decoder's reader.
pub async fn open(stream: SegmentStream, file: &NamedTempFile) -> Result<TrackReader> {
  let settings = Settings::default()
    .prefetch_bytes(PREFETCH_BYTES)
    .retry_timeout(STALL_TIMEOUT);
  let (reader, _) = crate::infra::progressive::open(stream, file, settings).await?;
  Ok(reader)
}

/// The track as a [`SegmentStream`]: the init's codec header, then each
/// audio segment fetched and decrypted.
pub fn segment_stream(
  http: Client,
  url_template: String,
  content_key: [u8; 16],
  init: &InitSegment,
) -> SegmentStream {
  let fetch = Arc::new(move |index: usize| {
    let http = http.clone();
    let template = url_template.clone();
    Box::pin(async move { fetch_audio_segment(&http, &template, index as u32, &content_key).await })
      as crate::infra::progressive::ChunkFuture
  });
  SegmentStream::new(
    Bytes::copy_from_slice(&init.header),
    init.segment_lengths.iter().map(|&len| u64::from(len)),
    fetch,
  )
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Seek, SeekFrom};

  use futures::StreamExt;
  use stream_download::source::SourceStream;

  use super::super::download::fetch_init;
  use super::super::download::test_support::*;
  use super::*;

  async fn fixture_stream(segments: Vec<Vec<u8>>) -> SegmentStream {
    let template = serve(segments).await;
    let http = Client::new();
    let init = fetch_init(&http, &template).await.unwrap();
    segment_stream(http, template, KEY, &init)
  }

  async fn collect(stream: &mut SegmentStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
      out.extend_from_slice(&chunk.unwrap());
    }
    out
  }

  #[tokio::test]
  async fn stream_yields_header_then_decrypted_segments() {
    let (segments, expected) = fixture_track();
    let mut stream = fixture_stream(segments).await;
    assert_eq!(stream.total_bytes(), expected.len() as u64);
    assert_eq!(collect(&mut stream).await, expected);
  }

  #[tokio::test]
  async fn seek_range_restarts_inside_a_segment_and_honors_the_end_bound() {
    let (segments, expected) = fixture_track();
    let mut stream = fixture_stream(segments).await;
    stream.seek_range(45, None).await.unwrap();
    assert_eq!(collect(&mut stream).await, expected[45..]);
    // Bytes 0..50 lie in the header and segment 1 (0..112): both are yielded.
    stream.seek_range(0, Some(50)).await.unwrap();
    assert_eq!(collect(&mut stream).await, expected[..112]);
    stream.seek_range(112, None).await.unwrap();
    assert_eq!(collect(&mut stream).await, expected[112..]);
  }

  #[tokio::test]
  async fn missing_segment_fails_after_the_last_attempt() {
    let (mut segments, expected) = fixture_track();
    segments.pop();
    let mut stream = fixture_stream(segments).await;
    let mut out = Vec::new();
    let mut errors = 0;
    while let Some(chunk) = stream.next().await {
      match chunk {
        Ok(bytes) => out.extend_from_slice(&bytes),
        Err(_) => errors += 1,
      }
    }
    assert_eq!(out, expected[..112]);
    assert_eq!(errors, crate::infra::progressive::MAX_ATTEMPTS);
    assert!(stream.seek_range(112, None).await.is_err());
  }

  #[tokio::test]
  async fn reader_returns_the_track_and_fills_the_tempfile() {
    let (segments, expected) = fixture_track();
    let stream = fixture_stream(segments).await;
    let tmp = NamedTempFile::new().unwrap();
    let mut reader = open(stream, &tmp).await.unwrap();
    let bytes = tokio::task::spawn_blocking(move || {
      let mut out = Vec::new();
      reader.read_to_end(&mut out).map(|_| out)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(bytes, expected);
    assert_eq!(std::fs::read(tmp.path()).unwrap(), expected);
  }

  #[tokio::test]
  async fn reader_seeks_within_the_track() {
    let (segments, expected) = fixture_track();
    let stream = fixture_stream(segments).await;
    let tmp = NamedTempFile::new().unwrap();
    let mut reader = open(stream, &tmp).await.unwrap();
    let tail_start = expected.len() - 15;
    let (tail, all) = tokio::task::spawn_blocking(move || {
      reader.seek(SeekFrom::Start(tail_start as u64)).unwrap();
      let mut tail = Vec::new();
      reader.read_to_end(&mut tail).unwrap();
      reader.seek(SeekFrom::Start(0)).unwrap();
      let mut all = Vec::new();
      reader.read_to_end(&mut all).unwrap();
      (tail, all)
    })
    .await
    .unwrap();
    assert_eq!(tail, expected[tail_start..]);
    assert_eq!(all, expected);
  }

  #[tokio::test]
  async fn reader_fails_when_a_segment_is_missing() {
    let (mut segments, _) = fixture_track();
    segments.pop();
    let stream = fixture_stream(segments).await;
    let tmp = NamedTempFile::new().unwrap();
    let mut reader = open(stream, &tmp).await.unwrap();
    let result = tokio::task::spawn_blocking(move || {
      let mut out = Vec::new();
      reader.read_to_end(&mut out)
    })
    .await
    .unwrap();
    assert!(result.is_err());
  }
}
