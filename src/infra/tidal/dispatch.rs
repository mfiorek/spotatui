//! Tidal browse/search/login routing.
//!
//! The seam that keeps the Spotify [`Network`](crate::infra::network)
//! Spotify-only: [`route_tidal_event`] is called from the runtime IoEvent pump
//! after the Qobuz dispatch and before Radio. An event that targets the Tidal
//! source (a browse, search or login request) is handled here and consumed;
//! anything else falls through. Playback is not wired up yet: the pump's
//! claim gate drops a `tidal:` start before any router sees it.
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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;

use super::auth::{self, ClientCredentials, DeviceLogin};
use super::TidalSource;
use crate::core::app::{App, TrackTableContext};
use crate::core::source::{MediaSource, Searcher};
use crate::infra::network::IoEvent;

/// How long the login URL stays in the status bar: the device code's lifetime.
const LOGIN_URL_TTL_SECS: u64 = 300;

const LOGIN_EXPIRED: &str = "Tidal: login expired, press `d` and pick Tidal to log in again";

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
  let mut app = app.lock().await;
  if auth::needs_login(&err) {
    log::info!("[tidal] {step}: {err}");
    super::set_login(None);
    app.set_error_status_message(LOGIN_EXPIRED, 8);
  } else {
    log::warn!("[tidal] {step}: {err:#}");
    app.set_status_message(format!("Tidal: {step}: {err:#}"), 6);
  }
}

/// A source for the current login: the in-memory one, else the saved one
/// restored silently. When only the user can help, `TidalLogin` is
/// dispatched (its success reloads the sidebar) and `None` returned.
async fn build_source(app: &Arc<Mutex<App>>) -> Option<TidalSource> {
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
      log::info!("[tidal] {e}; asking for a login");
      app.lock().await.dispatch(IoEvent::TidalLogin);
      None
    }
    Err(e) => {
      report(app, "login", e).await;
      None
    }
  }
}

// ---------------------------------------------------------------------------
// Browse + search
// ---------------------------------------------------------------------------

/// Fetch the sidebar rows (favorites, playlists, albums) into `app.tidal_playlists()`.
async fn load_tidal_playlists(app: &Arc<Mutex<App>>) {
  let Some(source) = build_source(app).await else {
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
  let Some(source) = build_source(app).await else {
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
  let Some(source) = build_source(app).await else {
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
// Login
// ---------------------------------------------------------------------------

static LOGIN_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

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
    set_status(app, "Tidal login already in progress...", 6).await;
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
  set_status(
    app,
    format!("Tidal: open {url} to log in (waiting up to 5 minutes)"),
    LOGIN_URL_TTL_SECS,
  )
  .await;

  match super::finish_login(&login).await {
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
