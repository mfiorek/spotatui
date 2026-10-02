//! The authenticated private-API client.
//!
//! One [`TidalClient`] per login holds the token state behind an async mutex.
//! A refresh runs while the mutex is held, so concurrent callers wait for it
//! and then see the new token instead of refreshing again (single flight).
//! Every rotated token is saved to the credentials file, so later launches
//! stay silent.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use reqwest::{Client, StatusCode};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::auth::{self, AuthEndpoints, ClientCredentials, TidalCredentials};

const API_BASE: &str = "https://api.tidal.com/v1";
/// The private API is only exercised by official clients; this is the version
/// header python-tidal sends.
const CLIENT_VERSION: &str = "2025.7.16";
/// How many times a 429 is retried before it becomes an error.
const RATE_LIMIT_RETRIES: u32 = 2;
/// The backoff per attempt when a 429 names no usable `Retry-After`.
const RATE_LIMIT_UNIT: Duration = Duration::from_secs(1);

/// An authenticated client for one login.
pub struct TidalClient {
  http: Client,
  api_base: String,
  endpoints: AuthEndpoints,
  client: ClientCredentials,
  credentials: Mutex<TidalCredentials>,
  /// The session id from the last `sessions` call; in memory only.
  session_id: std::sync::Mutex<Option<String>>,
  /// Where rotated tokens are saved; `None` keeps them in memory (tests and
  /// the live test).
  persist: Option<PathBuf>,
  /// [`RATE_LIMIT_UNIT`], shortened by the tests.
  rate_limit_unit: Duration,
}

impl TidalClient {
  pub fn new(
    client: ClientCredentials,
    credentials: TidalCredentials,
    persist: Option<PathBuf>,
  ) -> Self {
    TidalClient {
      http: super::shared_tidal_client(),
      api_base: API_BASE.to_string(),
      endpoints: AuthEndpoints::default(),
      client,
      credentials: Mutex::new(credentials),
      session_id: std::sync::Mutex::new(None),
      persist,
      rate_limit_unit: RATE_LIMIT_UNIT,
    }
  }

  #[cfg(test)]
  pub(super) fn at(base: &str, client: ClientCredentials, credentials: TidalCredentials) -> Self {
    TidalClient {
      http: Client::new(),
      api_base: format!("{base}/v1"),
      endpoints: AuthEndpoints::at(format!("{base}/oauth2")),
      rate_limit_unit: Duration::from_millis(1),
      ..TidalClient::new(client, credentials, None)
    }
  }

  /// The client this login belongs to.
  pub fn client_id(&self) -> &str {
    &self.client.id
  }

  /// The logged-in user's id, from the last session call.
  pub async fn user_id(&self) -> String {
    self.credentials.lock().await.user_id.clone()
  }

  /// A snapshot of the token state.
  #[cfg(test)]
  pub async fn credentials(&self) -> TidalCredentials {
    self.credentials.lock().await.clone()
  }

  fn save(&self, credentials: &TidalCredentials) {
    if let Some(path) = &self.persist {
      if let Err(e) = auth::write_credentials(path, credentials) {
        log::warn!("[tidal] cannot save credentials: {e:#}");
      }
    }
  }

  /// The `Authorization` value to send. Refreshes first when the token is
  /// missing, within the expiry slack, or equal to `stale` (the token a
  /// request was just refused with).
  async fn authorization(&self, stale: Option<&str>) -> Result<(String, String)> {
    let mut credentials = self.credentials.lock().await;
    let refused = stale.is_some_and(|s| s == credentials.access_token);
    if !refused && credentials.is_fresh_at(auth::unix_now()) {
      return Ok((
        credentials.access_token.clone(),
        format!("{} {}", credentials.token_type, credentials.access_token),
      ));
    }
    if credentials.refresh_token.is_empty() {
      return Err(auth::LoginRequired("Tidal is not logged in").into());
    }
    log::info!("[tidal] refreshing the access token");
    let token = auth::refresh(
      &self.http,
      &self.endpoints,
      &self.client,
      &credentials.refresh_token,
    )
    .await
    .map_err(auth::TokenError::into_anyhow)
    .context("Tidal token refresh")?;
    credentials.apply(token, auth::unix_now());
    self.save(&credentials);
    Ok((
      credentials.access_token.clone(),
      format!("{} {}", credentials.token_type, credentials.access_token),
    ))
  }

  /// GET `path` (relative to `/v1/`) and decode the JSON reply. The country
  /// code and session id ride along when known. A 401 forces one refresh and
  /// a retry; a second 401 asks for a new login. A 429 is retried
  /// [`RATE_LIMIT_RETRIES`] times after the server's backoff.
  pub(super) async fn get_json<T: DeserializeOwned>(
    &self,
    path: &str,
    params: &[(&str, String)],
  ) -> Result<T> {
    let url = format!("{}/{path}", self.api_base);
    let mut stale: Option<String> = None;
    let mut rate_limited = 0;
    loop {
      let (token, authorization) = self.authorization(stale.as_deref()).await?;
      let mut query: Vec<(&str, String)> = params.to_vec();
      let country_code = self.credentials.lock().await.country_code.clone();
      if !country_code.is_empty() {
        query.push(("countryCode", country_code));
      }
      if let Some(session_id) = self.session_id.lock().ok().and_then(|s| s.clone()) {
        query.push(("sessionId", session_id));
      }
      let response = self
        .http
        .get(&url)
        .query(&query)
        .header(reqwest::header::USER_AGENT, super::USER_AGENT)
        .header("x-tidal-client-version", CLIENT_VERSION)
        .header(reqwest::header::AUTHORIZATION, authorization)
        .send()
        .await
        .map_err(reqwest::Error::without_url)
        .with_context(|| format!("{path} request"))?;
      let status = response.status();
      if status == StatusCode::UNAUTHORIZED {
        if stale.is_some() {
          return Err(auth::LoginRequired("Tidal refused the login").into());
        }
        stale = Some(token);
        continue;
      }
      if status == StatusCode::TOO_MANY_REQUESTS && rate_limited < RATE_LIMIT_RETRIES {
        rate_limited += 1;
        let header = response
          .headers()
          .get(reqwest::header::RETRY_AFTER)
          .and_then(|v| v.to_str().ok());
        let delay = retry_delay(header, rate_limited, self.rate_limit_unit);
        log::info!("[tidal] {path} rate limited; retrying in {delay:?}");
        tokio::time::sleep(delay).await;
        continue;
      }
      let body = response
        .text()
        .await
        .map_err(reqwest::Error::without_url)
        .with_context(|| format!("{path} body"))?;
      if !status.is_success() {
        let excerpt: String = body.chars().take(120).collect();
        return Err(anyhow!("{path} returned HTTP {status}: {excerpt}"));
      }
      return serde_json::from_str(&body).with_context(|| format!("{path} decode"));
    }
  }

  /// Validate the token and fetch the user id, country code and session id
  /// the catalog endpoints need; the result is saved.
  pub async fn load_session(&self) -> Result<()> {
    let reply: SessionReply = self.get_json("sessions", &[]).await?;
    let user_id = match reply.user_id {
      serde_json::Value::Number(n) => n.to_string(),
      serde_json::Value::String(s) => s,
      _ => String::new(),
    };
    if user_id.is_empty() || reply.country_code.is_empty() {
      return Err(anyhow!("the Tidal session has no user or country"));
    }
    if let Ok(mut slot) = self.session_id.lock() {
      *slot = Some(reply.session_id).filter(|s| !s.is_empty());
    }
    let mut credentials = self.credentials.lock().await;
    credentials.user_id = user_id;
    credentials.country_code = reply.country_code;
    self.save(&credentials);
    Ok(())
  }
}

/// The backoff before retry `attempt` of a 429: the server's `Retry-After`
/// seconds when they are a sane 1..=60, else `attempt` times `unit`.
fn retry_delay(header: Option<&str>, attempt: u32, unit: Duration) -> Duration {
  match header.and_then(|v| v.trim().parse::<u64>().ok()) {
    Some(secs @ 1..=60) => Duration::from_secs(secs),
    _ => unit * attempt,
  }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionReply {
  #[serde(default)]
  session_id: String,
  #[serde(default)]
  country_code: String,
  #[serde(default)]
  user_id: serde_json::Value,
}

#[cfg(test)]
mod tests {
  use super::super::test_server::{serve, Reply};
  use super::*;

  const SESSION: &str = r#"{"sessionId":"s-1","countryCode":"NO","userId":12345}"#;
  const TOKEN: &str = r#"{"access_token":"fresh","token_type":"Bearer","expires_in":3600}"#;

  fn client() -> ClientCredentials {
    ClientCredentials {
      id: "client-id".into(),
      secret: "client-secret".into(),
    }
  }

  fn credentials(expires_at: u64) -> TidalCredentials {
    TidalCredentials {
      client_id: "client-id".into(),
      access_token: "old".into(),
      refresh_token: "refresh".into(),
      token_type: "Bearer".into(),
      expires_at,
      user_id: String::new(),
      country_code: String::new(),
    }
  }

  #[tokio::test]
  async fn a_fresh_token_loads_the_session_without_a_refresh() {
    let (base, server) = serve(vec![Reply::new("200 OK", SESSION)]).await;
    let session = TidalClient::at(&base, client(), credentials(auth::unix_now() + 3_600));
    session.load_session().await.unwrap();
    let saved = session.credentials().await;
    assert_eq!(saved.user_id, "12345");
    assert_eq!(saved.country_code, "NO");
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(
      requests[0].starts_with("GET /v1/sessions "),
      "{}",
      requests[0]
    );
    assert!(requests[0].contains("authorization: Bearer old"));
  }

  #[tokio::test]
  async fn an_expired_token_is_refreshed_before_the_call() {
    let (base, server) = serve(vec![
      Reply::new("200 OK", TOKEN),
      Reply::new("200 OK", SESSION),
    ])
    .await;
    let session = TidalClient::at(&base, client(), credentials(0));
    session.load_session().await.unwrap();
    let saved = session.credentials().await;
    assert_eq!(saved.access_token, "fresh");
    assert_eq!(saved.refresh_token, "refresh");
    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("POST /oauth2/token "));
    assert!(requests[0].contains("grant_type=refresh_token"));
    assert!(requests[1].contains("authorization: Bearer fresh"));
  }

  #[tokio::test]
  async fn a_401_forces_one_refresh_and_a_retry() {
    let (base, server) = serve(vec![
      Reply::new("401 Unauthorized", "{}"),
      Reply::new("200 OK", TOKEN),
      Reply::new("200 OK", SESSION),
    ])
    .await;
    let session = TidalClient::at(&base, client(), credentials(auth::unix_now() + 3_600));
    session.load_session().await.unwrap();
    assert_eq!(server.await.unwrap().len(), 3);
  }

  #[tokio::test]
  async fn concurrent_callers_share_one_refresh() {
    let (base, server) = serve(vec![
      Reply::new("200 OK", TOKEN),
      Reply::new("200 OK", SESSION),
      Reply::new("200 OK", SESSION),
    ])
    .await;
    let session = TidalClient::at(&base, client(), credentials(0));
    let (a, b) = tokio::join!(session.load_session(), session.load_session());
    a.unwrap();
    b.unwrap();
    let requests = server.await.unwrap();
    let refreshes = requests
      .iter()
      .filter(|r| r.starts_with("POST /oauth2/token "))
      .count();
    assert_eq!(refreshes, 1);
  }

  #[test]
  fn retry_delay_honours_a_sane_retry_after_and_scales_otherwise() {
    let unit = Duration::from_secs(1);
    assert_eq!(retry_delay(Some("5"), 1, unit), Duration::from_secs(5));
    assert_eq!(retry_delay(Some("0"), 2, unit), Duration::from_secs(2));
    assert_eq!(retry_delay(Some("600"), 1, unit), Duration::from_secs(1));
    assert_eq!(retry_delay(Some("soon"), 2, unit), Duration::from_secs(2));
    assert_eq!(retry_delay(None, 1, unit), Duration::from_secs(1));
  }

  #[tokio::test]
  async fn a_429_is_retried_after_the_backoff() {
    let (base, server) = serve(vec![
      Reply::new("429 Too Many Requests", "{}"),
      Reply::new("200 OK", SESSION),
    ])
    .await;
    let session = TidalClient::at(&base, client(), credentials(auth::unix_now() + 3_600));
    session.load_session().await.unwrap();
    assert_eq!(server.await.unwrap().len(), 2);
  }

  #[tokio::test]
  async fn a_429_past_the_retries_is_an_error() {
    let (base, _server) = serve(vec![
      Reply::new("429 Too Many Requests", "{}"),
      Reply::new("429 Too Many Requests", "{}"),
      Reply::new("429 Too Many Requests", "{}"),
    ])
    .await;
    let session = TidalClient::at(&base, client(), credentials(auth::unix_now() + 3_600));
    let err = session.load_session().await.unwrap_err();
    assert!(err.to_string().contains("429"), "{err:#}");
  }

  #[tokio::test]
  async fn invalid_grant_on_refresh_asks_for_a_new_login() {
    let (base, _server) = serve(vec![Reply::new(
      "400 Bad Request",
      r#"{"error":"invalid_grant","error_description":"Token has expired"}"#,
    )])
    .await;
    let session = TidalClient::at(&base, client(), credentials(0));
    let err = session.load_session().await.unwrap_err();
    assert!(auth::needs_login(&err), "{err:#}");
  }
}
