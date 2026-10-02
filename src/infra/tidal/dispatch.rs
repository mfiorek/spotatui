//! Tidal browse/search/login/playback routing.
//!
//! The seam that keeps the Spotify [`Network`](crate::infra::network)
//! Spotify-only: [`route_tidal_event`] is called from the runtime IoEvent pump
//! after the Qobuz dispatch and before Radio. An event that targets the Tidal
//! source (a browse, search or login request, or a `tidal:` playback URI) is
//! handled here and consumed; anything else falls through.
//!
//! ## Playback
//!
//! Tidal playback owns the private `App::tidal_playback` session and never
//! writes Spotify or librespot fields. A track plays while it downloads
//! (`super::stream`): the session is published at once, marked `advancing`,
//! and a detached task asks for the manifest, opens the CDN download and
//! builds the decoder, which waits for the first bytes. A skip during that
//! window supersedes it through `fetch_id`; the superseded reader is dropped,
//! which cancels its download. Repeat-one replays the tempfile only when the
//! download completed (a forward seek can leave a gap that was never filled);
//! otherwise it fetches the track again.
//!
//! The native queue suspends a Tidal session like any other decoded one: it
//! aborts the fetch in flight and borrows the session's player, and a fetch
//! that finishes while the queue owns the sink leaves the session alone for
//! the queue's resume. A queued Tidal track is downloaded whole first
//! ([`download_for_queue`]), as Qobuz's is.
//!
//! ## Browsing
//!
//! A browse uses the in-memory login, else restores the saved one inline,
//! else dispatches `TidalLogin`; a completed login reloads the sidebar. A
//! login the server refuses is cleared, so the next browse logs in again.
//!
//! ## Login
//!
//! `TidalLogin` first restores the saved login silently (a token refresh plus
//! the session call). Only when that needs the user does it run the device
//! flow: the `link.tidal.com` URL goes to the status bar, the browser opens
//! best-effort, and a detached task polls for the approval so the pump keeps
//! serving other events. Failures are status messages, never `handle_error`:
//! the CLI never reaches this router, so no exit signal is lost.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tempfile::NamedTempFile;
use tokio::sync::Mutex;

use super::auth::{self, ClientCredentials, DeviceLogin};
use super::dash;
use super::manifest::{self, Delivered, StreamKind, StreamSource};
use super::{track_id_from_uri, ResumePoint, TidalPlaybackState, TidalSource};
use crate::core::app::{App, TrackTableContext};
use crate::core::source::{MediaSource, Searcher, Source};
use crate::core::state::PersistedRuntimeState;
use crate::infra::audio::{LocalPlayer, PreparedStream};
use crate::infra::network::IoEvent;
use crate::infra::progressive::{Completion, TrackReader};
use crate::infra::queue::{advance_index, replay_file, snapshot_tracks};

/// How long the login URL stays in the status bar: the device code's lifetime.
const LOGIN_URL_TTL_SECS: u64 = 300;

const LOGIN_EXPIRED: &str = "Tidal: login expired, press `d` and pick Tidal to log in again";

/// Whether a URI is owned by the Tidal source.
pub fn is_tidal_uri(uri: &str) -> bool {
  uri.starts_with("tidal:")
}

/// Skip direction within the queue.
#[derive(Clone, Copy)]
enum Direction {
  Next,
  Prev,
}

/// Intercept events that target the Tidal source.
///
/// Returns `true` if the event was handled (and must **not** be forwarded to
/// the Spotify network), `false` to let the normal dispatch run.
pub async fn route_tidal_event(app: &Arc<Mutex<App>>, event: &IoEvent) -> bool {
  match event {
    IoEvent::GetTidalPlaylists => {
      load_tidal_playlists(app).await;
      true
    }
    IoEvent::GetTidalTracks(uri) => {
      load_tidal_tracks(app, uri).await;
      true
    }
    IoEvent::GetTidalSearchResults(query) => {
      run_tidal_search(app, query).await;
      true
    }
    IoEvent::TidalLogin => {
      begin_login(app).await;
      true
    }
    // Start a list of Tidal tracks: queue all and start at the offset.
    IoEvent::StartPlayback(None, Some(uris), offset)
      if uris.first().is_some_and(|u| is_tidal_uri(u)) =>
    {
      start_tidal_queue(app, uris, offset.unwrap_or(0), None).await;
      true
    }
    // A single Tidal track with no surrounding list: a one-track queue.
    IoEvent::StartPlayback(Some(uri), _, _) if is_tidal_uri(uri) => {
      start_tidal_queue(app, std::slice::from_ref(uri), 0, None).await;
      true
    }
    // Bare "resume current": ours only while Tidal owns the session.
    IoEvent::StartPlayback(None, None, None) => match player(app).await {
      Some(p) => {
        p.resume();
        true
      }
      None => false,
    },
    // Any other start is a foreign play: relinquish the device, then let the
    // normal dispatch run.
    IoEvent::StartPlayback(..) => {
      teardown_tidal(app).await;
      false
    }
    IoEvent::PausePlayback => match player(app).await {
      Some(p) => {
        p.pause();
        true
      }
      None => false,
    },
    IoEvent::Seek(position_ms) => match player(app).await {
      Some(p) => {
        // A seek past the downloaded part waits for a range request: off the pump.
        let position = Duration::from_millis(*position_ms as u64);
        tokio::task::spawn_blocking(move || {
          let _ = p.seek(position);
        });
        true
      }
      None => false,
    },
    IoEvent::ChangeVolume(volume) => match player(app).await {
      Some(p) => {
        p.set_volume(*volume);
        let mut app = app.lock().await;
        app.runtime_state.volume_percent = *volume;
        app.schedule_state_save(PersistedRuntimeState::volume_percent(*volume));
        true
      }
      None => false,
    },
    IoEvent::NextTrack => skip(app, Direction::Next).await,
    IoEvent::PreviousTrack | IoEvent::ForcePreviousTrack => skip(app, Direction::Prev).await,
    IoEvent::ReplayCurrentTrack => replay_current(app).await,
    IoEvent::Repeat(state) => app.lock().await.set_decoded_repeat_from_state(*state),
    _ => false,
  }
}

/// Deliberate divergence from `handle_error`: a status message, no error route.
async fn set_status(app: &Arc<Mutex<App>>, message: impl Into<String>, ttl_secs: u64) {
  app
    .lock()
    .await
    .set_status_message(message.into(), ttl_secs);
}

/// Report a failed call as one status message; a refused login is also
/// cleared, so the next browse runs the login again.
async fn report(app: &Arc<Mutex<App>>, step: &str, err: anyhow::Error) {
  let mut guard = app.lock().await;
  report_locked(&mut guard, step, err);
}

fn report_locked(app: &mut App, step: &str, err: anyhow::Error) {
  if auth::needs_login(&err) {
    log::info!("[tidal] {step}: {err}");
    super::set_login(None);
    app.set_error_status_message(LOGIN_EXPIRED, 8);
  } else {
    log::warn!("[tidal] {step}: {err:#}");
    app.set_status_message(format!("Tidal: {step}: {err:#}"), 6);
  }
}

/// What to do when only a new login can help.
#[derive(Clone, Copy)]
enum WhenLoggedOut {
  /// Browse paths start the device login.
  Login,
  /// Playback paths only show the login message.
  Message,
}

/// A source for the current login: the in-memory one, else the saved one
/// restored silently. When only the user can help, the logged-out case is
/// handled as `when_logged_out` says and `None` returned; a dispatched
/// `TidalLogin` reloads the sidebar on success.
async fn build_source(
  app: &Arc<Mutex<App>>,
  when_logged_out: WhenLoggedOut,
) -> Option<TidalSource> {
  let client = auth::client_credentials(&app.lock().await.user_config.behavior);
  let Some(client) = client else {
    set_status(app, auth::NO_CLIENT_ID, 10).await;
    return None;
  };
  if let Some(login) = super::current_login().filter(|login| login.client_id() == client.id) {
    return Some(TidalSource::new(login));
  }
  match super::restore_login(client).await {
    Ok(login) => Some(TidalSource::new(login)),
    Err(e) if auth::needs_login(&e) => {
      match when_logged_out {
        WhenLoggedOut::Login => {
          log::info!("[tidal] {e}; asking for a login");
          app.lock().await.dispatch(IoEvent::TidalLogin);
        }
        WhenLoggedOut::Message => report(app, "login", e).await,
      }
      None
    }
    Err(e) => {
      report(app, "login", e).await;
      None
    }
  }
}

/// A source for the native queue's off-pump fetch; a missing login is only
/// reported.
pub(crate) async fn build_playback_source(app: &Arc<Mutex<App>>) -> Option<TidalSource> {
  build_source(app, WhenLoggedOut::Message).await
}

// ---------------------------------------------------------------------------
// Browse + search
// ---------------------------------------------------------------------------

/// Fetch the sidebar rows (favorites, playlists, albums) into `app.tidal_playlists()`.
async fn load_tidal_playlists(app: &Arc<Mutex<App>>) {
  let Some(source) = build_source(app, WhenLoggedOut::Login).await else {
    return;
  };
  match source.playlists().await {
    Ok(playlists) => *app.lock().await.tidal_playlists_mut() = playlists,
    Err(e) => report(app, "library", e).await,
  }
}

/// Fetch a listing's tracks into the shared track table, tagged
/// [`TrackTableContext::TidalPlaylist`].
async fn load_tidal_tracks(app: &Arc<Mutex<App>>, playlist_uri: &str) {
  let Some(source) = build_source(app, WhenLoggedOut::Login).await else {
    return;
  };
  match source.tracks(playlist_uri).await {
    Ok(tracks) => {
      app.lock().await.set_source_track_table(
        playlist_uri,
        tracks,
        TrackTableContext::TidalPlaylist,
      );
    }
    Err(e) => report(app, "tracks", e).await,
  }
}

/// Run a catalog search and populate the songs block of `app.search_results`.
async fn run_tidal_search(app: &Arc<Mutex<App>>, query: &str) {
  let Some(source) = build_source(app, WhenLoggedOut::Login).await else {
    return;
  };
  match source.search(query).await {
    Ok(results) => app
      .lock()
      .await
      .show_source_search_tracks(query, results.tracks),
    Err(e) => report(app, "search", e).await,
  }
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

/// The live Tidal player, if a Tidal session is active.
async fn player(app: &Arc<Mutex<App>>) -> Option<Arc<LocalPlayer>> {
  app
    .lock()
    .await
    .tidal_playback()
    .map(|s| Arc::clone(&s.player))
}

/// Release every other backend so only Tidal holds the output device.
async fn release_other_backends(app: &Arc<Mutex<App>>) {
  // Take the sink from native Spotify so no rebuild resumes it under this
  // source.
  #[cfg(feature = "streaming")]
  app.lock().await.release_native_for_decoded();
  // The other decoded sources never see this `tidal:` start (the pump
  // short-circuits), so their sessions are torn down here.
  let players = app.lock().await.take_decoded_sessions_except(Source::Tidal);
  for player in players {
    player.stop_detached();
  }
}

/// Reuse the live Tidal player, or open a fresh output device for one. A
/// freshly opened player is **not** published to `App` here.
async fn acquire_player(app: &Arc<Mutex<App>>) -> Option<Arc<LocalPlayer>> {
  if let Some(p) = player(app).await {
    return Some(p);
  }
  match tokio::task::spawn_blocking(LocalPlayer::new).await {
    Ok(Ok(p)) => Some(Arc::new(p)),
    Ok(Err(e)) => {
      set_status(app, format!("No audio output for Tidal playback: {e}"), 6).await;
      None
    }
    Err(e) => {
      set_status(app, format!("Audio output init failed: {e}"), 6).await;
      None
    }
  }
}

static FETCH_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_fetch_id() -> u64 {
  FETCH_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// A track whose download runs and whose decoder is built.
pub(super) struct PreparedTrack {
  pub(super) tempfile: NamedTempFile,
  complete: Completion,
  pub(super) delivered: Delivered,
  stream: PreparedStream,
}

/// Ask for the track in hi-res and run `step` on the stream. A hi-res (DASH)
/// stream whose step fails is asked for again as HIGH, which always comes
/// over BTS.
async fn with_fallback<T, F, Fut>(source: &TidalSource, track_id: &str, step: F) -> Result<T>
where
  F: Fn(StreamSource) -> Fut,
  Fut: std::future::Future<Output = Result<T>>,
{
  let stream_source = source
    .stream_source(track_id, manifest::REQUESTED_QUALITY)
    .await?;
  if !matches!(stream_source.kind, StreamKind::Dash { .. }) {
    return step(stream_source).await;
  }
  match step(stream_source).await {
    Ok(done) => Ok(done),
    Err(e) => {
      log::warn!(
        "[tidal] track {track_id}: the hi-res stream failed ({e:#}); asking for {}",
        manifest::FALLBACK_QUALITY
      );
      let fallback = source
        .stream_source(track_id, manifest::FALLBACK_QUALITY)
        .await?;
      step(fallback).await
    }
  }
}

/// Open the track's download into a fresh tempfile and build its decoder,
/// whose first read waits for the prefetch.
pub(super) async fn prepare_track(source: &TidalSource, track_id: &str) -> Result<PreparedTrack> {
  with_fallback(source, track_id, |stream_source| {
    prepare_source(track_id, stream_source)
  })
  .await
}

/// Download the track at `uri` whole into a tempfile, for the native queue
/// engine's off-pump fetch (playing is the queue's job), and return its
/// delivered format label.
pub(crate) async fn download_for_queue(
  source: &TidalSource,
  uri: &str,
) -> Result<(NamedTempFile, String)> {
  let track_id = track_id_from_uri(uri)?;
  let (tempfile, delivered) = with_fallback(source, track_id, |stream_source| {
    download_source(track_id, stream_source)
  })
  .await?;
  Ok((tempfile, delivered.label()))
}

/// A stream downloading into its tempfile.
struct OpenedSource {
  reader: TrackReader,
  complete: Completion,
  mime: Option<String>,
  byte_len: Option<u64>,
  delivered: Delivered,
}

/// Start downloading `stream_source` into `tempfile`.
async fn open_source(
  stream_source: StreamSource,
  tempfile: &NamedTempFile,
) -> Result<OpenedSource> {
  Ok(match stream_source.kind {
    StreamKind::Bts { url, mime_type } => {
      let opened = super::stream::open(&url, tempfile).await?;
      OpenedSource {
        reader: opened.reader,
        complete: opened.completion,
        mime: mime_type,
        byte_len: opened.byte_len,
        delivered: stream_source.delivered,
      }
    }
    StreamKind::Dash { mpd } => {
      let manifest = dash::parse_mpd(&mpd)?;
      let opened = super::segments::open(&manifest, tempfile).await?;
      OpenedSource {
        reader: opened.reader,
        complete: opened.completion,
        mime: Some("audio/flac".to_string()),
        byte_len: Some(opened.byte_len),
        delivered: Delivered::Flac(opened.format),
      }
    }
  })
}

/// Open `stream_source` into a fresh tempfile and build its decoder.
async fn prepare_source(track_id: &str, stream_source: StreamSource) -> Result<PreparedTrack> {
  let tempfile = NamedTempFile::new().context("creating temp file for Tidal stream")?;
  let OpenedSource {
    reader,
    complete,
    mime,
    byte_len,
    delivered,
  } = open_source(stream_source, &tempfile).await?;
  let stream = tokio::task::spawn_blocking(move || {
    LocalPlayer::prepare_stream(reader, mime.as_deref(), byte_len)
  })
  .await
  .context("decoder task")??;
  log::info!("[tidal] track {track_id} delivered {}", delivered.label());
  Ok(PreparedTrack {
    tempfile,
    complete,
    delivered,
    stream,
  })
}

/// Download `stream_source` into a fresh tempfile, reading it to its end.
async fn download_source(
  track_id: &str,
  stream_source: StreamSource,
) -> Result<(NamedTempFile, Delivered)> {
  let tempfile = NamedTempFile::new().context("creating temp file for Tidal stream")?;
  let OpenedSource {
    mut reader,
    delivered,
    ..
  } = open_source(stream_source, &tempfile).await?;
  tokio::task::spawn_blocking(move || std::io::copy(&mut reader, &mut std::io::sink()))
    .await
    .context("download task")?
    .context("downloading the Tidal stream")?;
  log::info!(
    "[tidal] queued track {track_id} delivered {}",
    delivered.label()
  );
  Ok((tempfile, delivered))
}

/// Fetch the track on a detached task, then play it when the session still
/// waits for this fetch. Called under the `App` lock that stamped
/// `session.fetch_id`, so the abort handle is in place before any skip looks
/// for it: a skip, a new queue or a teardown cancels the task, and a decoder
/// build already in progress ends on its own and drops its reader, which
/// cancels that download.
fn spawn_fetch(app: &Arc<Mutex<App>>, session: &mut TidalPlaybackState, track_id: String) {
  let app = Arc::clone(app);
  let source = Arc::clone(&session.source);
  let fetch_id = session.fetch_id;
  let task = tokio::spawn(async move {
    match prepare_track(&source, &track_id).await {
      Ok(prepared) => commit_fetch(&app, fetch_id, prepared).await,
      Err(e) => fail_fetch(&app, fetch_id, "stream", e).await,
    }
  });
  session.fetch = Some(task.abort_handle());
}

/// A failed fetch: tear the session down only if it still waits for this
/// fetch, and report the error. A teardown rather than a skip: a skip would
/// walk the list at tick speed when every track fails the same way. Under the
/// native queue the session stays for the queue's resume, which fetches again.
async fn fail_fetch(app: &Arc<Mutex<App>>, fetch_id: u64, step: &str, err: anyhow::Error) {
  let mut guard = app.lock().await;
  if guard
    .tidal_playback()
    .is_none_or(|s| s.fetch_id != fetch_id)
  {
    return;
  }
  if guard.queue_owns_playback() {
    log::warn!("[tidal] {step}: {err:#}");
    return;
  }
  let session = guard.set_tidal_playback(None);
  report_locked(&mut guard, step, err);
  drop(guard);
  if let Some(s) = session {
    Arc::clone(&s.player).stop_detached();
  }
}

/// Play the prepared stream and finalize the session. The previous track is
/// cleared off the `App` lock first (the sink clear waits for the audio
/// thread, which a stalled stream holds), then the session is checked again
/// under the lock so a concurrent skip (which restamps `fetch_id`) cannot
/// interleave. A stale stream is dropped, which cancels its download.
async fn commit_fetch(app: &Arc<Mutex<App>>, fetch_id: u64, prepared: PreparedTrack) {
  let claimed = {
    let guard = app.lock().await;
    // The native queue owns the sink: the session stays for its resume.
    if guard.queue_owns_playback() {
      return;
    }
    guard
      .tidal_playback()
      .filter(|s| s.fetch_id == fetch_id)
      // A pause pressed during the fetch window applies to the previous
      // track's sink; a fresh player starts paused, so only a session that
      // already played something counts.
      .map(|s| {
        (
          Arc::clone(&s.player),
          s.tempfile.is_some() && s.player.is_paused(),
        )
      })
  };
  let Some((player, was_paused)) = claimed else {
    return;
  };
  let stop_player = Arc::clone(&player);
  if tokio::task::spawn_blocking(move || stop_player.stop())
    .await
    .is_err()
  {
    return;
  }
  let PreparedTrack {
    tempfile,
    complete,
    delivered,
    stream,
  } = prepared;
  // Claim the session under the lock, stage off it: the clear inside
  // `stage_prepared` waits on the audio thread, and the runner takes this
  // lock on every frame.
  let (resume, volume) = {
    let mut guard = app.lock().await;
    let volume = guard.runtime_state.volume_percent;
    let Some(s) = guard
      .tidal_playback_mut()
      .filter(|s| s.fetch_id == fetch_id)
    else {
      return;
    };
    s.tempfile = Some(tempfile);
    s.complete = Some(complete);
    s.quality = Some(delivered);
    s.fetch = None;
    (s.resume_at.take(), volume)
  };
  let paused = was_paused || resume.is_some_and(|r| r.paused);
  let stage_player = Arc::clone(&player);
  let staged = tokio::task::spawn_blocking(move || {
    stage_player.stage_prepared(stream)?;
    stage_player.set_volume(volume);
    if !paused {
      stage_player.resume();
    }
    Ok::<(), anyhow::Error>(())
  })
  .await;
  let staged = match staged {
    Ok(Ok(())) => true,
    Ok(Err(e)) => {
      log::warn!("[tidal] stage: {e:#}");
      false
    }
    Err(e) => {
      log::warn!("[tidal] stage task: {e}");
      false
    }
  };
  let mut guard = app.lock().await;
  let Some(s) = guard
    .tidal_playback_mut()
    .filter(|s| s.fetch_id == fetch_id)
  else {
    return;
  };
  if !staged {
    // The device went under the stage. Replay instead, with the same pause;
    // the advance latch stays on until it does.
    s.resume_at = Some(ResumePoint {
      position_ms: resume.map_or(0, |r| r.position_ms),
      paused,
    });
    guard.dispatch(IoEvent::ReplayCurrentTrack);
    return;
  }
  s.advancing = false;
  let display = s.current().map(|t| t.name.clone());
  if let Some(display) = display {
    guard.set_status_message(format!("\u{266a} {display}"), 4);
  }
  drop(guard);
  // The restore seek waits for that part of the download: off the App lock.
  if let Some(position_ms) = resume.map(|r| r.position_ms).filter(|&ms| ms > 0) {
    tokio::task::spawn_blocking(move || {
      let _ = player.seek(Duration::from_millis(position_ms));
    });
  }
}

/// Begin playing a list of Tidal tracks, taking over the session and starting
/// at `start_idx` (clamped into range). `resume` is applied when the first
/// track plays (session restore), so it is in place before the fetch starts.
pub(crate) async fn start_tidal_queue(
  app: &Arc<Mutex<App>>,
  uris: &[String],
  start_idx: usize,
  resume: Option<ResumePoint>,
) {
  let tracks = {
    let guard = app.lock().await;
    let search = guard
      .search_results()
      .tracks
      .as_ref()
      .map(|p| p.items.as_slice());
    snapshot_tracks(&guard.track_table.tracks, search, uris)
  };
  if tracks.is_empty() {
    set_status(app, "No Tidal tracks to play", 6).await;
    return;
  }
  let index = start_idx.min(tracks.len() - 1);
  let track_id = match tracks[index].uri.as_deref().map(track_id_from_uri) {
    Some(Ok(id)) => id.to_string(),
    _ => {
      set_status(app, "Invalid Tidal track URI", 6).await;
      return;
    }
  };
  let Some(source) = build_source(app, WhenLoggedOut::Message).await else {
    return;
  };
  let source = Arc::new(source);

  // Only one backend owns the device at a time.
  app.lock().await.claim_decoded_sink(Source::Tidal);
  release_other_backends(app).await;
  let Some(player) = acquire_player(app).await else {
    return;
  };

  // Publish the session now, marked advancing, so the playbar and the skip
  // keys see it during the download; `commit_fetch` finalizes it.
  let mut guard = app.lock().await;
  let mut state = TidalPlaybackState {
    player,
    source,
    tracks,
    index,
    advancing: true,
    tempfile: None,
    complete: None,
    quality: None,
    shuffle_backup: None,
    fetch_id: next_fetch_id(),
    resume_at: resume,
    fetch: None,
  };
  // Honor the player-global decoded shuffle for the freshly built queue.
  if guard.decoded_shuffle {
    state.set_shuffle(true);
  }
  spawn_fetch(app, &mut state, track_id);
  // Dropping a previous session aborts its download.
  guard.set_tidal_playback(Some(state));
}

/// Move the queue index in `direction` and play the new track. Returns `true`
/// if Tidal owns the session (so the event is consumed).
async fn skip(app: &Arc<Mutex<App>>, direction: Direction) -> bool {
  let target = {
    let mut guard = app.lock().await;
    let mode = guard.decoded_repeat;
    let Some(s) = guard.tidal_playback_mut() else {
      return false;
    };
    s.advancing = true;
    let forward = matches!(direction, Direction::Next);
    advance_index(s.index, s.tracks.len(), mode, forward)
  };
  match target {
    Some(idx) => play_index(app, idx, None).await,
    None => {
      // Queue boundary: clear the guard so auto-advance is not wedged off. A
      // pending first download keeps it (the sink is still empty).
      if let Some(s) = app.lock().await.tidal_playback_mut() {
        if s.tempfile.is_some() {
          s.advancing = false;
        }
      }
    }
  }
  true
}

/// How a replay of the current track proceeds.
enum Replay {
  /// The file is whole: restage it, with no second download.
  File(Arc<LocalPlayer>, std::path::PathBuf, Option<ResumePoint>),
  /// The file has a gap (or its download stopped early): fetch it again.
  Fetch(usize, ResumePoint),
  /// Still downloading: the commit applies `resume_at` and starts playback.
  Pending,
}

/// Replay the current track (repeat-one, device recovery). Returns `true` if
/// Tidal owns the session.
async fn replay_current(app: &Arc<Mutex<App>>) -> bool {
  let replay = {
    let mut guard = app.lock().await;
    let Some(s) = guard.tidal_playback_mut() else {
      return false;
    };
    s.advancing = true;
    if s.tempfile.is_none() {
      Replay::Pending
    } else {
      let resume = s.resume_at.take().unwrap_or(ResumePoint {
        position_ms: 0,
        paused: s.player.is_paused(),
      });
      match s.tempfile.as_ref() {
        Some(t) if s.file_is_complete() => {
          Replay::File(Arc::clone(&s.player), t.path().to_path_buf(), Some(resume))
        }
        _ => Replay::Fetch(s.index, resume),
      }
    }
  };
  match replay {
    Replay::File(player, path, resume) => {
      if replay_file(player, path, resume).await {
        if let Some(s) = app.lock().await.tidal_playback_mut() {
          s.advancing = false;
        }
      } else {
        teardown_tidal(app).await;
        set_status(app, "Cannot replay Tidal track", 6).await;
      }
    }
    Replay::Fetch(index, resume) => play_index(app, index, Some(resume)).await,
    Replay::Pending => {}
  }
  true
}

/// Play the queued track at `target` in the published session: the index moves
/// at once and the download runs off the pump. Used by Next/Previous, the tick's
/// auto-advance, a replay that fetches again, and the native queue's resume;
/// the last two pass how the track starts as `resume`.
pub(crate) async fn play_index(app: &Arc<Mutex<App>>, target: usize, resume: Option<ResumePoint>) {
  let mut guard = app.lock().await;
  let Some(s) = guard.tidal_playback_mut() else {
    return; // session torn down between dispatch and here
  };
  let Some(track) = s.tracks.get(target) else {
    s.advancing = false;
    return;
  };
  let track_id = match track.uri.as_deref().map(track_id_from_uri) {
    Some(Ok(id)) => id.to_string(),
    _ => {
      drop(guard);
      teardown_tidal(app).await;
      set_status(app, "Invalid Tidal track URI", 6).await;
      return;
    }
  };
  // Cancel a superseded download; the file, format and restore point of the
  // previous track go with it, so the session never describes two tracks.
  if let Some(fetch) = s.fetch.take() {
    fetch.abort();
  }
  s.tempfile = None;
  s.complete = None;
  s.quality = None;
  s.resume_at = resume;
  s.index = target;
  s.advancing = true;
  s.fetch_id = next_fetch_id();
  spawn_fetch(app, s, track_id);
}

/// End the Tidal session, releasing the output device and the tempfile. The
/// player stops off the `App` lock.
async fn teardown_tidal(app: &Arc<Mutex<App>>) {
  let session = app.lock().await.set_tidal_playback(None);
  if let Some(s) = session {
    Arc::clone(&s.player).stop_detached_holding(s);
  }
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

static LOGIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);
/// The URL of the device login waiting for the user, and when it was shown;
/// a repeated login request shows it again instead of replacing it.
static PENDING_LOGIN: std::sync::Mutex<Option<(String, Instant)>> = std::sync::Mutex::new(None);

fn set_pending_login(pending: Option<(String, Instant)>) {
  *PENDING_LOGIN.lock().unwrap_or_else(|e| e.into_inner()) = pending;
}

/// The status line of a login waiting for the user, and its remaining TTL.
fn login_url_status(url: &str, shown_at: Instant, now: Instant) -> (String, u64) {
  let elapsed = now.saturating_duration_since(shown_at).as_secs();
  (
    format!("Tidal: open {url} to log in (waiting up to 5 minutes)"),
    LOGIN_URL_TTL_SECS.saturating_sub(elapsed).max(1),
  )
}

/// Make sure a login is in place, on a detached task so the pump keeps running.
async fn begin_login(app: &Arc<Mutex<App>>) {
  let client = auth::client_credentials(&app.lock().await.user_config.behavior);
  let Some(client) = client else {
    set_status(app, auth::NO_CLIENT_ID, 10).await;
    return;
  };
  if super::current_login().is_some_and(|login| login.client_id() == client.id) {
    return;
  }
  if LOGIN_IN_PROGRESS.swap(true, Ordering::SeqCst) {
    let pending = PENDING_LOGIN
      .lock()
      .unwrap_or_else(|e| e.into_inner())
      .clone();
    match pending {
      Some((url, shown_at)) => {
        let (message, ttl) = login_url_status(&url, shown_at, Instant::now());
        set_status(app, message, ttl).await;
      }
      None => set_status(app, "Tidal login already in progress...", 6).await,
    }
    return;
  }
  let app = Arc::clone(app);
  tokio::spawn(async move {
    run_login(&app, client).await;
    LOGIN_IN_PROGRESS.store(false, Ordering::SeqCst);
  });
}

/// Reload the sidebar once a login is in place. Sent on the channel rather
/// than through `dispatch`, so no spinner is pinned from this task.
async fn reload_sidebar(app: &Arc<Mutex<App>>) {
  if let Some(tx) = app.lock().await.io_tx_clone() {
    let _ = tx.send(IoEvent::GetTidalPlaylists);
  }
}

/// Restore the saved login, or run the device flow when only the user can help.
async fn run_login(app: &Arc<Mutex<App>>, client: ClientCredentials) {
  match super::restore_login(client.clone()).await {
    Ok(_) => {
      log::info!("[tidal] restored the saved login");
      reload_sidebar(app).await;
      return;
    }
    Err(e) if auth::needs_login(&e) => log::info!("[tidal] {e}; starting the device login"),
    Err(e) => {
      log::warn!("[tidal] restoring the login: {e:#}");
      set_status(app, format!("Tidal: {e:#}"), 8).await;
      return;
    }
  }

  let login = match DeviceLogin::start(client).await {
    Ok(login) => login,
    Err(e) => {
      log::warn!("[tidal] device login: {e:#}");
      set_status(app, format!("Tidal login failed: {e:#}"), 10).await;
      return;
    }
  };
  let url = login.url().to_string();
  if let Err(e) = open::that_detached(&url) {
    log::warn!("[tidal] failed to open the browser: {e}");
  }
  let shown_at = Instant::now();
  let (message, ttl) = login_url_status(&url, shown_at, shown_at);
  set_status(app, message, ttl).await;
  set_pending_login(Some((url, shown_at)));

  let outcome = super::finish_login(&login).await;
  set_pending_login(None);
  match outcome {
    Ok(_) => {
      set_status(app, "Tidal: logged in", 4).await;
      reload_sidebar(app).await;
    }
    Err(e) => {
      log::warn!("[tidal] login failed: {e:#}");
      set_status(app, format!("Tidal login failed: {e:#}"), 10).await;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_repeated_login_request_shows_the_url_for_the_time_left() {
    let shown_at = Instant::now();
    let (message, ttl) = login_url_status(
      "https://link.tidal.com/ABCDE",
      shown_at,
      shown_at + Duration::from_secs(100),
    );
    assert!(
      message.contains("https://link.tidal.com/ABCDE"),
      "{message}"
    );
    assert_eq!(ttl, LOGIN_URL_TTL_SECS - 100);
  }

  #[test]
  fn an_expired_login_url_still_shows_for_a_moment() {
    let shown_at = Instant::now();
    let (_, ttl) = login_url_status("u", shown_at, shown_at + Duration::from_secs(900));
    assert_eq!(ttl, 1);
  }
}
