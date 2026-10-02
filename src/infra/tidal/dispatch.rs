//! Tidal event routing.
//!
//! The seam that keeps the Spotify [`Network`](crate::infra::network)
//! Spotify-only: [`route_tidal_event`] is called from the runtime IoEvent pump
//! after the Qobuz dispatch and before Radio. An event that targets the Tidal
//! source is handled here and consumed; anything else falls through.
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
use crate::core::app::App;
use crate::infra::network::IoEvent;

/// How long the login URL stays in the status bar: the device code's lifetime.
const LOGIN_URL_TTL_SECS: u64 = 300;

/// Intercept events that target the Tidal source.
///
/// Returns `true` if the event was handled (and must **not** be forwarded to
/// the Spotify network), `false` to let the normal dispatch run.
pub async fn route_tidal_event(app: &Arc<Mutex<App>>, event: &IoEvent) -> bool {
  match event {
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

/// Restore the saved login, or run the device flow when only the user can help.
async fn run_login(app: &Arc<Mutex<App>>, client: ClientCredentials) {
  match super::restore_login(client.clone()).await {
    Ok(_) => {
      log::info!("[tidal] restored the saved login");
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
    Ok(_) => set_status(app, "Tidal: logged in", 4).await,
    Err(e) => {
      log::warn!("[tidal] login failed: {e:#}");
      set_status(app, format!("Tidal login failed: {e:#}"), 10).await;
    }
  }
}
