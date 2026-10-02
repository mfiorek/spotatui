//! Progressive delivery: a track plays while it downloads into a tempfile.
//!
//! Shared by the sources that stream a finite file (Qobuz over its segment
//! transport, Tidal over a plain HTTP download). [`open`] runs a
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

use std::fs::File;
use std::io::{self, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use stream_download::source::SourceStream;
use stream_download::storage::StorageProvider;
use stream_download::{Settings, StreamDownload, StreamPhase};
use tempfile::NamedTempFile;

/// The reader the decoder pulls from; dropping it cancels the download.
pub type TrackReader = StreamDownload<TempfileStorage>;

/// Set once every byte of the stream is in the tempfile, gaps included.
#[derive(Clone, Default)]
pub struct Completion(Arc<AtomicBool>);

impl Completion {
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
