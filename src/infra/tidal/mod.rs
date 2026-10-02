//! Tidal media source.
//!
//! Browses the user's Tidal library through the private client API
//! (`api.tidal.com/v1`, the one the python-tidal ecosystem uses) and plays
//! tracks through the shared [`LocalPlayer`](crate::infra::audio::LocalPlayer).
//! The official developer API serves third-party clients 30-second previews
//! only.
//!
//! ## URIs
//!
//! Tracks: `tidal:track:<id>`. Sidebar rows (each opens the shared track table):
//! `tidal:favorites:tracks`, `tidal:playlist:<uuid>`, `tidal:album:<id>`.

pub mod auth;
pub mod client;
pub mod dispatch;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::Client;

use auth::{ClientCredentials, DeviceLogin, TidalCredentials};
use client::TidalClient;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// The Android WebView user agent python-tidal sends; the private API is only
/// exercised by official clients, so a generic one would stand out.
const USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 12; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/91.0.4472.114 Safari/537.36";

/// The process-wide HTTP client for auth and API calls.
pub fn shared_tidal_client() -> Client {
  static CLIENT: std::sync::OnceLock<Client> = std::sync::OnceLock::new();
  CLIENT
    .get_or_init(|| {
      Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .unwrap_or_default()
    })
    .clone()
}

fn login_slot() -> &'static std::sync::Mutex<Option<Arc<TidalClient>>> {
  static LOGIN: std::sync::OnceLock<std::sync::Mutex<Option<Arc<TidalClient>>>> =
    std::sync::OnceLock::new();
  LOGIN.get_or_init(|| std::sync::Mutex::new(None))
}

/// The login in use, when one was restored or completed in this process.
pub fn current_login() -> Option<Arc<TidalClient>> {
  login_slot().lock().ok().and_then(|slot| slot.clone())
}

/// Replace the in-memory login: `Some` after a login, `None` once it expired.
pub fn set_login(login: Option<Arc<TidalClient>>) {
  if let Ok(mut slot) = login_slot().lock() {
    *slot = login;
  }
}

/// Restore the saved login for `client` without user interaction: refresh the
/// token when needed and validate it with the session call. Fails with
/// [`auth::LoginRequired`] when only a new device login can help.
pub async fn restore_login(client: ClientCredentials) -> Result<Arc<TidalClient>> {
  let saved = auth::usable_credentials(auth::load_credentials(), &client)?;
  let login = Arc::new(TidalClient::new(
    client,
    saved,
    crate::core::paths::tidal_credentials_path(),
  ));
  login.load_session().await?;
  set_login(Some(Arc::clone(&login)));
  Ok(login)
}

/// Wait for a started device login, load its session, and save it.
pub async fn finish_login(login: &DeviceLogin) -> Result<Arc<TidalClient>> {
  let token = login.wait().await?;
  let client = login.client().clone();
  let credentials = TidalCredentials::from_token(&client.id, token, auth::unix_now());
  let path = crate::core::paths::tidal_credentials_path()
    .ok_or_else(|| anyhow!("no config directory to save the Tidal login in"))?;
  let session = Arc::new(TidalClient::new(client, credentials, Some(path)));
  session.load_session().await?;
  set_login(Some(Arc::clone(&session)));
  Ok(session)
}

/// A loopback HTTP server for the auth and API tests.
#[cfg(test)]
pub(crate) mod test_server {
  use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
  use tokio::net::TcpListener;

  /// One canned reply.
  #[derive(Clone)]
  pub struct Reply {
    status: &'static str,
    body: String,
  }

  impl Reply {
    pub fn new(status: &'static str, body: impl Into<String>) -> Self {
      Reply {
        status,
        body: body.into(),
      }
    }
  }

  /// Serve `replies` in order, one per connection. Returns the base URL and a
  /// handle yielding each request as its request line, headers (names
  /// lowercased) and body.
  pub async fn serve(replies: Vec<Reply>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
      let mut requests = Vec::new();
      for reply in replies {
        let Ok((mut stream, _)) = listener.accept().await else {
          break;
        };
        let (read_half, mut write_half) = stream.split();
        let mut reader = BufReader::new(read_half);
        let mut request = String::new();
        let mut content_length = 0usize;
        let mut first = true;
        loop {
          let mut line = String::new();
          if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
            break;
          }
          if line == "\r\n" {
            break;
          }
          let line = if first {
            first = false;
            line
          } else {
            match line.split_once(':') {
              Some((name, value)) => format!("{}:{value}", name.to_ascii_lowercase()),
              None => line,
            }
          };
          if let Some(value) = line.strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
          }
          request.push_str(&line);
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
          let _ = reader.read_exact(&mut body).await;
        }
        request.push_str(&String::from_utf8_lossy(&body));
        requests.push(request);
        let response = format!(
          "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
          reply.status,
          reply.body.len(),
          reply.body
        );
        let _ = write_half.write_all(response.as_bytes()).await;
        let _ = write_half.flush().await;
      }
      requests
    });
    (base, handle)
  }
}
