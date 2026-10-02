//! Progressive delivery: a track plays while it downloads into a tempfile.
//!
//! Shared by the sources that stream a finite file (Qobuz over its segment
//! transport, Tidal over a plain HTTP download or DASH segments). [`open`] runs a
//! `stream-download` source into the session's own `NamedTempFile`
//! ([`TempfileStorage`]): the decoder's reads block until their bytes exist,
//! a seek past the downloaded part restarts the source there, and dropping the
//! reader stops the download. The session keeps the `NamedTempFile`, so the
//! file outlives the reader (repeat-one).
//!
//! A forward seek leaves a hole that `stream-download` fills once the source
//! reaches the end. [`Completion`] says whether that happened: a reader
//! dropped before then (the track ended or was skipped) leaves a file with an
//! unfilled gap that must not be replayed.
//!
//! [`SegmentStream`] is the source for a track cut into segments whose sizes
//! are known up front: the codec header, then each segment's audio bytes as
//! the source's fetch function returns them.

use std::convert::Infallible;
use std::fs::File;
use std::future::Future;
use std::io::{self, BufReader};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::{anyhow, Context as _, Result};
use bytes::Bytes;
use futures::Stream;
use stream_download::source::SourceStream;
use stream_download::storage::StorageProvider;
use stream_download::{Settings, StreamDownload, StreamPhase};
use tempfile::NamedTempFile;
use tokio::task::JoinHandle;

/// The reader the decoder pulls from; dropping it cancels the download.
pub type TrackReader = StreamDownload<TempfileStorage>;

/// Set once every byte of the stream is in the tempfile, gaps included.
#[derive(Clone, Default)]
pub struct Completion(Arc<AtomicBool>);

impl Completion {
  // Qobuz drops its `Completion` until its repeat-one replay checks it.
  #[cfg_attr(not(feature = "tidal"), allow(dead_code))]
  pub fn is_complete(&self) -> bool {
    self.0.load(Ordering::Acquire)
  }

  fn mark(&self) {
    self.0.store(true, Ordering::Release);
  }
}

/// Start downloading `stream` into `file` and return the decoder's reader with
/// its [`Completion`]. `settings` carries the source's prefetch and stall
/// timeout; the progress callback is installed here.
pub async fn open<S>(
  stream: S,
  file: &NamedTempFile,
  settings: Settings<S>,
) -> Result<(TrackReader, Completion)>
where
  S: SourceStream,
  S::Error: std::fmt::Debug + Send,
{
  let storage = TempfileStorage::new(file).context("reopening stream file")?;
  let completion = Completion::default();
  let marker = completion.clone();
  let settings = settings.on_progress(move |_, state, _| {
    if state.phase == StreamPhase::Complete {
      marker.mark();
    }
  });
  let reader = StreamDownload::from_stream(stream, storage, settings)
    .await
    .map_err(|e| anyhow!("{e}"))?;
  Ok((reader, completion))
}

/// Storage over the session's own tempfile: one reopened handle for the
/// download's writes and one for the decoder's reads.
pub struct TempfileStorage {
  reader: File,
  writer: File,
}

impl TempfileStorage {
  fn new(file: &NamedTempFile) -> io::Result<Self> {
    Ok(Self {
      reader: file.reopen()?,
      writer: file.reopen()?,
    })
  }
}

impl StorageProvider for TempfileStorage {
  type Reader = BufReader<File>;
  type Writer = File;

  fn into_reader_writer(
    self,
    _content_length: Option<u64>,
  ) -> io::Result<(Self::Reader, Self::Writer)> {
    Ok((BufReader::new(self.reader), self.writer))
  }
}

/// Consecutive failed fetches of one segment before the stream fails.
pub(crate) const MAX_ATTEMPTS: u32 = 3;

/// A segment fetch in flight.
pub type ChunkFuture = Pin<Box<dyn Future<Output = Result<Bytes>> + Send>>;
/// Fetch the audio bytes of segment `i` (1 or later), exactly as many as the
/// length table promised.
pub type FetchChunk = Arc<dyn Fn(usize) -> ChunkFuture + Send + Sync>;

/// A track as a byte stream: chunk 0 is the codec header, chunk `i >= 1` is
/// the audio of segment `i`. Restarts at any byte offset when the decoder
/// seeks past the downloaded part.
pub struct SegmentStream {
  fetch: FetchChunk,
  header: Bytes,
  /// `starts[i]` is the byte offset of chunk `i`; the last entry is the total.
  starts: Vec<u64>,
  /// The next chunk to yield.
  next: usize,
  /// Bytes to drop from the front of the next chunk (a seek into a chunk).
  skip: usize,
  /// Chunks at or past this index are not yielded until the next seek.
  end: usize,
  in_flight: Option<JoinHandle<Result<Bytes>>>,
  attempts: u32,
  failure: Option<String>,
}

impl SegmentStream {
  /// A stream of `header` followed by segments of `lengths` bytes each.
  pub fn new(header: Bytes, lengths: impl IntoIterator<Item = u64>, fetch: FetchChunk) -> Self {
    let mut starts = vec![0u64, header.len() as u64];
    let mut position = header.len() as u64;
    for len in lengths {
      position += len;
      starts.push(position);
    }
    let chunks = starts.len() - 1;
    Self {
      fetch,
      header,
      starts,
      next: 0,
      skip: 0,
      end: chunks,
      in_flight: None,
      attempts: 0,
      failure: None,
    }
  }

  fn chunk_count(&self) -> usize {
    self.starts.len() - 1
  }

  /// The size of the finished file.
  pub fn total_bytes(&self) -> u64 {
    self.starts[self.chunk_count()]
  }

  /// The chunk that holds byte `position` and the offset inside that chunk;
  /// `(chunk_count, 0)` at or past the end.
  fn locate(&self, position: u64) -> (usize, usize) {
    let chunks = self.chunk_count();
    let index = self.starts[1..].partition_point(|&end| end <= position);
    if index >= chunks {
      (chunks, 0)
    } else {
      (index, (position - self.starts[index]) as usize)
    }
  }

  /// The exclusive chunk bound that covers the exclusive byte bound `end`.
  fn chunk_bound(&self, end: Option<u64>) -> usize {
    match end {
      None => self.chunk_count(),
      Some(0) => 0,
      Some(end) => (self.locate(end - 1).0 + 1).min(self.chunk_count()),
    }
  }

  /// Continue from byte `start`, yielding chunks below `end_chunk` only.
  fn restart(&mut self, start: u64, end_chunk: usize) -> io::Result<()> {
    if let Some(message) = &self.failure {
      return Err(io::Error::other(message.clone()));
    }
    self.abort_in_flight();
    let (chunk, skip) = self.locate(start);
    self.next = chunk;
    self.skip = skip;
    self.end = end_chunk;
    self.attempts = 0;
    Ok(())
  }

  fn abort_in_flight(&mut self) {
    if let Some(task) = self.in_flight.take() {
      task.abort();
    }
  }
}

impl Drop for SegmentStream {
  fn drop(&mut self) {
    self.abort_in_flight();
  }
}

impl Stream for SegmentStream {
  type Item = Result<Bytes>;

  fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    let this = &mut *self;
    if this.failure.is_some() || this.next >= this.end {
      return Poll::Ready(None);
    }
    let chunk = if this.next == 0 {
      this.header.clone()
    } else {
      let index = this.next;
      if this.in_flight.is_none() {
        this.in_flight = Some(tokio::spawn((this.fetch)(index)));
      }
      let task = this.in_flight.as_mut().expect("fetch task was just set");
      let outcome = match Pin::new(task).poll(cx) {
        Poll::Pending => return Poll::Pending,
        Poll::Ready(Ok(outcome)) => outcome,
        Poll::Ready(Err(join)) => Err(anyhow!("segment {index} fetch task: {join}")),
      };
      this.in_flight = None;
      match outcome {
        Ok(bytes) => bytes,
        Err(e) => {
          // `stream-download` logs the error and polls again; the failure
          // flag ends the stream after the last attempt.
          this.attempts += 1;
          if this.attempts >= MAX_ATTEMPTS {
            this.failure = Some(format!("{e:#}"));
          }
          return Poll::Ready(Some(Err(e)));
        }
      }
    };
    let skip = this.skip.min(chunk.len());
    this.next += 1;
    this.skip = 0;
    this.attempts = 0;
    Poll::Ready(Some(Ok(chunk.slice(skip..))))
  }
}

impl SourceStream for SegmentStream {
  type Params = Self;
  type StreamCreationError = Infallible;

  async fn create(params: Self) -> Result<Self, Infallible> {
    Ok(params)
  }

  fn content_length(&self) -> Option<u64> {
    Some(self.total_bytes())
  }

  async fn seek_range(&mut self, start: u64, end: Option<u64>) -> io::Result<()> {
    let bound = self.chunk_bound(end);
    self.restart(start, bound)
  }

  async fn reconnect(&mut self, current_position: u64) -> io::Result<()> {
    let bound = self.end;
    self.restart(current_position, bound)
  }

  fn supports_seek(&self) -> bool {
    true
  }
}

#[cfg(test)]
mod tests {
  use std::io::{Read, Seek, SeekFrom};

  use futures::StreamExt;
  use tempfile::NamedTempFile;

  use super::*;

  /// A stream over in-memory segments; a missing segment fails every fetch.
  fn stream_over(header: &[u8], segments: Vec<Vec<u8>>, lengths: &[u64]) -> SegmentStream {
    let segments = Arc::new(segments);
    let fetch: FetchChunk = Arc::new(move |i| {
      let segment = segments.get(i - 1).cloned();
      Box::pin(async move {
        segment
          .map(Bytes::from)
          .ok_or_else(|| anyhow!("no segment {i}"))
      })
    });
    SegmentStream::new(
      Bytes::copy_from_slice(header),
      lengths.iter().copied(),
      fetch,
    )
  }

  /// A 10-byte header and two segments of 70 and 20 bytes.
  fn track() -> (SegmentStream, Vec<u8>) {
    let header: Vec<u8> = (0u8..10).collect();
    let one: Vec<u8> = (100u8..170).collect();
    let two: Vec<u8> = (200u8..220).collect();
    let expected = [header.clone(), one.clone(), two.clone()].concat();
    (stream_over(&header, vec![one, two], &[70, 20]), expected)
  }

  async fn collect(stream: &mut SegmentStream) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
      out.extend_from_slice(&chunk.unwrap());
    }
    out
  }

  #[tokio::test]
  async fn locate_maps_offsets_to_chunks() {
    let (s, _) = track();
    assert_eq!(s.total_bytes(), 100);
    assert_eq!(s.locate(0), (0, 0));
    assert_eq!(s.locate(9), (0, 9));
    assert_eq!(s.locate(10), (1, 0));
    assert_eq!(s.locate(45), (1, 35));
    assert_eq!(s.locate(80), (2, 0));
    assert_eq!(s.locate(99), (2, 19));
    assert_eq!(s.locate(100), (3, 0));
    assert_eq!(s.locate(500), (3, 0));
  }

  #[tokio::test]
  async fn chunk_bound_covers_the_chunk_that_holds_the_last_byte() {
    let (s, _) = track();
    assert_eq!(s.chunk_bound(None), 3);
    assert_eq!(s.chunk_bound(Some(0)), 0);
    assert_eq!(s.chunk_bound(Some(10)), 1);
    assert_eq!(s.chunk_bound(Some(11)), 2);
    assert_eq!(s.chunk_bound(Some(100)), 3);
    assert_eq!(s.chunk_bound(Some(1000)), 3);
  }

  #[tokio::test]
  async fn the_stream_yields_the_header_then_each_segment() {
    let (mut stream, expected) = track();
    assert_eq!(collect(&mut stream).await, expected);
  }

  #[tokio::test]
  async fn seek_range_restarts_inside_a_segment_and_honors_the_end_bound() {
    let (mut stream, expected) = track();
    stream.seek_range(45, None).await.unwrap();
    assert_eq!(collect(&mut stream).await, expected[45..]);
    // Bytes 0..50 lie in the header and segment 1 (0..80): both are yielded.
    stream.seek_range(0, Some(50)).await.unwrap();
    assert_eq!(collect(&mut stream).await, expected[..80]);
    stream.seek_range(80, None).await.unwrap();
    assert_eq!(collect(&mut stream).await, expected[80..]);
  }

  #[tokio::test]
  async fn a_missing_segment_fails_the_stream_after_the_last_attempt() {
    let header: Vec<u8> = (0u8..10).collect();
    let one: Vec<u8> = (100u8..170).collect();
    let mut stream = stream_over(&header, vec![one.clone()], &[70, 20]);
    let mut out = Vec::new();
    let mut errors = 0;
    while let Some(chunk) = stream.next().await {
      match chunk {
        Ok(bytes) => out.extend_from_slice(&bytes),
        Err(_) => errors += 1,
      }
    }
    assert_eq!(out, [header, one].concat());
    assert_eq!(errors, MAX_ATTEMPTS);
    assert!(stream.seek_range(80, None).await.is_err());
  }

  #[tokio::test]
  async fn the_reader_seeks_within_the_track_and_fills_the_tempfile() {
    let (stream, expected) = track();
    let tmp = NamedTempFile::new().unwrap();
    let (mut reader, completion) = open(stream, &tmp, Settings::default()).await.unwrap();
    let (tail, all) = tokio::task::spawn_blocking(move || {
      reader.seek(SeekFrom::Start(85)).unwrap();
      let mut tail = Vec::new();
      reader.read_to_end(&mut tail).unwrap();
      reader.seek(SeekFrom::Start(0)).unwrap();
      let mut all = Vec::new();
      reader.read_to_end(&mut all).unwrap();
      (tail, all)
    })
    .await
    .unwrap();
    assert_eq!(tail, expected[85..]);
    assert_eq!(all, expected);
    for _ in 0..500 {
      if completion.is_complete() {
        break;
      }
      tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(completion.is_complete());
    assert_eq!(std::fs::read(tmp.path()).unwrap(), expected);
  }
}
