//! The OAuth client, the device-flow login, and the credentials file.
//!
//! The client ID and secret are never embedded: they come from
//! `behavior.tidal_client_id` / `tidal_client_secret` in `config.yml`, with the
//! `SPOTATUI_TIDAL_CLIENT_*` env vars taking precedence. The credentials file
//! holds machine-written token state only, plus the client ID that minted the
//! tokens (a refresh must use the same client); the secret is never written.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::core::user_config::BehaviorConfig;

const AUTH_BASE: &str = "https://auth.tidal.com/v1/oauth2";
const SCOPE: &str = "r_usr w_usr w_sub";
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

const CLIENT_ID_ENV: &str = "SPOTATUI_TIDAL_CLIENT_ID";
const CLIENT_SECRET_ENV: &str = "SPOTATUI_TIDAL_CLIENT_SECRET";

/// How long the device login may wait for approval, whatever the server says.
const LOGIN_CAP: Duration = Duration::from_secs(5 * 60);
/// RFC 8628 §3.2: the poll interval when the server names none.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);
/// RFC 8628 §3.5: `slow_down` widens the interval by this much.
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// A token this close to its expiry is refreshed before use.
pub(super) const EXPIRY_SLACK_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// Client ID
// ---------------------------------------------------------------------------

/// The OAuth client pair every token request sends.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientCredentials {
  pub id: String,
  pub secret: String,
}

impl std::fmt::Debug for ClientCredentials {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("ClientCredentials")
      .field("id", &self.id)
      .finish_non_exhaustive()
  }
}

/// The client pair: each env value over its config value, and the ID as the
/// secret when no secret is set. `None` when no ID is set anywhere.
fn client_credentials_with(
  env_id: Option<String>,
  env_secret: Option<String>,
  config_id: Option<String>,
  config_secret: Option<String>,
) -> Option<ClientCredentials> {
  let non_empty = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
  let id = non_empty(env_id).or_else(|| non_empty(config_id))?;
  let secret = non_empty(env_secret)
    .or_else(|| non_empty(config_secret))
    .unwrap_or_else(|| id.clone());
  Some(ClientCredentials { id, secret })
}

/// The configured client pair, if any.
pub fn client_credentials(behavior: &BehaviorConfig) -> Option<ClientCredentials> {
  client_credentials_with(
    std::env::var(CLIENT_ID_ENV).ok(),
    std::env::var(CLIENT_SECRET_ENV).ok(),
    behavior.tidal_client_id.clone(),
    behavior.tidal_client_secret.clone(),
  )
}

/// What to tell a user who has not configured a client ID.
pub const NO_CLIENT_ID: &str =
  "Tidal: set behavior.tidal_client_id in config.yml (or SPOTATUI_TIDAL_CLIENT_ID) to log in";

// ---------------------------------------------------------------------------
// Login-required error
// ---------------------------------------------------------------------------

/// The saved login cannot be used: the user must run the device login again.
#[derive(Debug)]
pub struct LoginRequired(pub &'static str);

impl std::fmt::Display for LoginRequired {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.0)
  }
}

impl std::error::Error for LoginRequired {}

/// Whether `err` asks for a new login.
pub fn needs_login(err: &anyhow::Error) -> bool {
  err.downcast_ref::<LoginRequired>().is_some()
}

// ---------------------------------------------------------------------------
// Credentials file
// ---------------------------------------------------------------------------

/// The saved login. `expires_at` is in Unix seconds.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TidalCredentials {
  pub client_id: String,
  pub access_token: String,
  pub refresh_token: String,
  #[serde(default = "bearer")]
  pub token_type: String,
  #[serde(default)]
  pub expires_at: u64,
  #[serde(default)]
  pub user_id: String,
  #[serde(default)]
  pub country_code: String,
}

impl std::fmt::Debug for TidalCredentials {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("TidalCredentials")
      .field("client_id", &self.client_id)
      .field("expires_at", &self.expires_at)
      .field("user_id", &self.user_id)
      .field("country_code", &self.country_code)
      .finish_non_exhaustive()
  }
}

fn bearer() -> String {
  "Bearer".to_string()
}

impl TidalCredentials {
  /// Fresh credentials for `client_id` from a token reply received at `now`;
  /// the user id and country code arrive with the session call.
  pub(super) fn from_token(client_id: &str, token: Token, now: u64) -> Self {
    let mut credentials = TidalCredentials {
      client_id: client_id.to_string(),
      access_token: String::new(),
      refresh_token: String::new(),
      token_type: bearer(),
      expires_at: 0,
      user_id: String::new(),
      country_code: String::new(),
    };
    credentials.apply(token, now);
    credentials
  }

  /// Store a token reply. A refresh reply usually omits the refresh token, so
  /// the old one is kept then.
  pub(super) fn apply(&mut self, token: Token, now: u64) {
    self.access_token = token.access_token;
    self.token_type = token.token_type.unwrap_or_else(bearer);
    if let Some(refresh) = token.refresh_token.filter(|t| !t.is_empty()) {
      self.refresh_token = refresh;
    }
    self.expires_at = now.saturating_add(token.expires_in);
  }

  /// Whether the access token can still be sent at `now`, with
  /// [`EXPIRY_SLACK_SECS`] to spare.
  pub(super) fn is_fresh_at(&self, now: u64) -> bool {
    !self.access_token.is_empty() && now.saturating_add(EXPIRY_SLACK_SECS) < self.expires_at
  }
}

/// Seconds since the Unix epoch.
pub(super) fn unix_now() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0)
}

/// Read the credentials file; `None` when missing or unreadable.
fn read_credentials(path: &Path) -> Option<TidalCredentials> {
  std::fs::read_to_string(path)
    .ok()
    .and_then(|text| serde_yaml::from_str::<TidalCredentials>(&text).ok())
}

/// Write the credentials file with private permissions.
pub(super) fn write_credentials(path: &Path, credentials: &TidalCredentials) -> Result<()> {
  if let Some(dir) = path.parent() {
    crate::core::paths::ensure_private_dir(dir)?;
  }
  let yaml = serde_yaml::to_string(credentials).context("serializing Tidal credentials")?;
  crate::core::auth::write_private_file_atomic(path, yaml.as_bytes())
    .with_context(|| format!("writing {}", path.display()))
}

/// The saved credentials, if the file exists and parses.
pub(super) fn load_credentials() -> Option<TidalCredentials> {
  crate::core::paths::tidal_credentials_path().and_then(|p| read_credentials(&p))
}

/// The saved credentials when they can serve `client`; a login is required
/// when nothing is saved, the refresh token is missing, or another client ID
/// minted the tokens (a refresh must use the same client).
pub(super) fn usable_credentials(
  saved: Option<TidalCredentials>,
  client: &ClientCredentials,
) -> Result<TidalCredentials, LoginRequired> {
  let saved = saved.ok_or(LoginRequired("Tidal is not logged in"))?;
  if saved.refresh_token.is_empty() {
    return Err(LoginRequired("the saved Tidal login is incomplete"));
  }
  if saved.client_id != client.id {
    return Err(LoginRequired(
      "the Tidal client ID changed since the last login",
    ));
  }
  Ok(saved)
}

// ---------------------------------------------------------------------------
// Token endpoint
// ---------------------------------------------------------------------------

/// Where the OAuth endpoints live; tests point it at a loopback server.
#[derive(Clone, Debug)]
pub(super) struct AuthEndpoints {
  base: String,
}

impl Default for AuthEndpoints {
  fn default() -> Self {
    AuthEndpoints {
      base: AUTH_BASE.to_string(),
    }
  }
}

impl AuthEndpoints {
  #[cfg(test)]
  pub(super) fn at(base: impl Into<String>) -> Self {
    AuthEndpoints { base: base.into() }
  }

  fn device_authorization(&self) -> String {
    format!("{}/device_authorization", self.base)
  }

  fn token(&self) -> String {
    format!("{}/token", self.base)
  }
}

/// A successful token reply.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Token {
  pub access_token: String,
  pub refresh_token: Option<String>,
  pub token_type: Option<String>,
  pub expires_in: u64,
}

impl std::fmt::Debug for Token {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Token")
      .field("expires_in", &self.expires_in)
      .finish_non_exhaustive()
  }
}

/// The raw `oauth2/token` body. OAuth errors arrive with a 4xx status and an
/// `error` field, so the body is decoded whatever the status.
#[derive(Default, Deserialize)]
struct TokenReply {
  access_token: Option<String>,
  refresh_token: Option<String>,
  token_type: Option<String>,
  expires_in: Option<u64>,
  error: Option<String>,
  error_description: Option<String>,
}

/// Why a token request returned no token.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TokenError {
  /// RFC 8628 `authorization_pending`: the user has not approved yet.
  Pending,
  /// RFC 8628 `slow_down`: poll less often.
  SlowDown,
  /// The grant is dead (`invalid_grant`, `expired_token`): log in again.
  Revoked(String),
  /// Anything else, including transport failures.
  Failed(String),
}

impl std::fmt::Display for TokenError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      TokenError::Pending => f.write_str("authorization pending"),
      TokenError::SlowDown => f.write_str("polling too fast"),
      TokenError::Revoked(why) => write!(f, "login expired ({why})"),
      TokenError::Failed(why) => f.write_str(why),
    }
  }
}

impl std::error::Error for TokenError {}

impl TokenError {
  /// The anyhow form: a revoked grant becomes [`LoginRequired`].
  pub(super) fn into_anyhow(self) -> anyhow::Error {
    match self {
      TokenError::Revoked(why) => {
        log::warn!("[tidal] token revoked: {why}");
        LoginRequired("the Tidal login expired").into()
      }
      other => anyhow!("{other}"),
    }
  }
}

/// Map a token reply to a token or a [`TokenError`].
fn classify(reply: TokenReply) -> Result<Token, TokenError> {
  let describe = |code: &str, description: Option<String>| match description {
    Some(d) if !d.is_empty() => format!("{code}: {d}"),
    _ => code.to_string(),
  };
  match reply.error.as_deref() {
    None | Some("") => {}
    Some("authorization_pending") => return Err(TokenError::Pending),
    Some("slow_down") => return Err(TokenError::SlowDown),
    Some(code @ ("invalid_grant" | "expired_token")) => {
      return Err(TokenError::Revoked(describe(code, reply.error_description)))
    }
    Some(code) => return Err(TokenError::Failed(describe(code, reply.error_description))),
  }
  let access_token = reply
    .access_token
    .filter(|t| !t.is_empty())
    .ok_or_else(|| TokenError::Failed("token reply has no access_token".to_string()))?;
  Ok(Token {
    access_token,
    refresh_token: reply.refresh_token,
    token_type: reply.token_type.filter(|t| !t.is_empty()),
    expires_in: reply.expires_in.unwrap_or(0),
  })
}

/// POST a form to an auth endpoint and decode its JSON body whatever the status.
async fn post_form<T: serde::de::DeserializeOwned>(
  http: &Client,
  url: &str,
  form: &[(&str, &str)],
) -> Result<T> {
  let response = http
    .post(url)
    .header(reqwest::header::USER_AGENT, super::USER_AGENT)
    .form(form)
    .send()
    .await
    .map_err(reqwest::Error::without_url)
    .context("Tidal auth request")?;
  let status = response.status();
  let body = response
    .text()
    .await
    .map_err(reqwest::Error::without_url)
    .context("Tidal auth reply")?;
  serde_json::from_str(&body).map_err(|e| {
    let excerpt: String = body.chars().take(120).collect();
    anyhow!("Tidal auth returned HTTP {status} ({e}): {excerpt}")
  })
}

/// One token request.
async fn request_token(
  http: &Client,
  endpoints: &AuthEndpoints,
  form: &[(&str, &str)],
) -> Result<Token, TokenError> {
  let reply: TokenReply = post_form(http, &endpoints.token(), form)
    .await
    .map_err(|e| TokenError::Failed(format!("{e:#}")))?;
  classify(reply)
}

/// Trade a refresh token for a new access token.
pub(super) async fn refresh(
  http: &Client,
  endpoints: &AuthEndpoints,
  client: &ClientCredentials,
  refresh_token: &str,
) -> Result<Token, TokenError> {
  request_token(
    http,
    endpoints,
    &[
      ("grant_type", "refresh_token"),
      ("refresh_token", refresh_token),
      ("client_id", &client.id),
      ("client_secret", &client.secret),
      ("scope", SCOPE),
    ],
  )
  .await
}

// ---------------------------------------------------------------------------
// Device login
// ---------------------------------------------------------------------------

/// The `device_authorization` reply. `verificationUriComplete` already embeds
/// the user code (`link.tidal.com/ABCDE`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceAuthorization {
  #[serde(default)]
  device_code: String,
  #[serde(default)]
  verification_uri_complete: String,
  #[serde(default)]
  expires_in: u64,
  #[serde(default)]
  interval: u64,
}

/// How the poll loop paces itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PollTiming {
  interval: Duration,
  slow_down_step: Duration,
  cap: Duration,
}

impl PollTiming {
  /// The server's interval and lifetime (seconds; 0 when absent), with the
  /// RFC 8628 default interval and [`LOGIN_CAP`] as the upper bound.
  fn from_server(interval: u64, expires_in: u64) -> Self {
    let interval = match interval {
      0 => DEFAULT_POLL_INTERVAL,
      secs => Duration::from_secs(secs),
    };
    let cap = match expires_in {
      0 => LOGIN_CAP,
      secs => Duration::from_secs(secs).min(LOGIN_CAP),
    };
    PollTiming {
      interval,
      slow_down_step: SLOW_DOWN_STEP,
      cap,
    }
  }
}

/// A started device login: show [`url`](Self::url), then [`wait`](Self::wait).
pub struct DeviceLogin {
  http: Client,
  endpoints: AuthEndpoints,
  client: ClientCredentials,
  device_code: String,
  url: String,
  timing: PollTiming,
}

impl DeviceLogin {
  /// Ask Tidal for a device code.
  pub async fn start(client: ClientCredentials) -> Result<Self> {
    Self::start_at(
      super::shared_tidal_client(),
      AuthEndpoints::default(),
      client,
    )
    .await
  }

  async fn start_at(
    http: Client,
    endpoints: AuthEndpoints,
    client: ClientCredentials,
  ) -> Result<Self> {
    let reply: DeviceAuthorization = post_form(
      &http,
      &endpoints.device_authorization(),
      &[("client_id", &client.id), ("scope", SCOPE)],
    )
    .await?;
    if reply.device_code.is_empty() || reply.verification_uri_complete.is_empty() {
      return Err(anyhow!(
        "Tidal refused the device login (the client ID may be revoked; check behavior.tidal_client_id)"
      ));
    }
    let url = if reply.verification_uri_complete.starts_with("http") {
      reply.verification_uri_complete
    } else {
      format!("https://{}", reply.verification_uri_complete)
    };
    Ok(DeviceLogin {
      http,
      endpoints,
      client,
      device_code: reply.device_code,
      url,
      timing: PollTiming::from_server(reply.interval, reply.expires_in),
    })
  }

  /// The `link.tidal.com` URL the user opens, code included.
  pub fn url(&self) -> &str {
    &self.url
  }

  /// The client this login runs for.
  pub fn client(&self) -> &ClientCredentials {
    &self.client
  }

  /// Poll until the user approves the device, the code expires, or the
  /// five-minute cap passes.
  pub(super) async fn wait(&self) -> Result<Token> {
    poll_device_token(
      &self.http,
      &self.endpoints,
      &self.client,
      &self.device_code,
      self.timing,
    )
    .await
  }
}

/// The RFC 8628 poll loop: `authorization_pending` waits another interval,
/// `slow_down` widens it, anything else ends the loop.
async fn poll_device_token(
  http: &Client,
  endpoints: &AuthEndpoints,
  client: &ClientCredentials,
  device_code: &str,
  timing: PollTiming,
) -> Result<Token> {
  let form = [
    ("grant_type", DEVICE_CODE_GRANT),
    ("device_code", device_code),
    ("client_id", &client.id),
    ("client_secret", &client.secret),
    ("scope", SCOPE),
  ];
  let deadline = tokio::time::Instant::now() + timing.cap;
  let mut interval = timing.interval;
  loop {
    if tokio::time::Instant::now() + interval > deadline {
      return Err(anyhow!(
        "the Tidal login code expired before it was approved"
      ));
    }
    tokio::time::sleep(interval).await;
    match request_token(http, endpoints, &form).await {
      Ok(token) => return Ok(token),
      Err(TokenError::Pending) => {}
      Err(TokenError::SlowDown) => interval += timing.slow_down_step,
      Err(TokenError::Revoked(_)) => {
        return Err(anyhow!(
          "the Tidal login code expired before it was approved"
        ))
      }
      Err(e) => return Err(e.into_anyhow()),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::super::test_server::{serve, Reply};
  use super::*;

  fn client() -> ClientCredentials {
    ClientCredentials {
      id: "client-id".into(),
      secret: "client-secret".into(),
    }
  }

  fn saved(client_id: &str) -> TidalCredentials {
    TidalCredentials {
      client_id: client_id.into(),
      access_token: "access".into(),
      refresh_token: "refresh".into(),
      token_type: "Bearer".into(),
      expires_at: 1_000,
      user_id: "42".into(),
      country_code: "NO".into(),
    }
  }

  fn reply(error: &str, description: Option<&str>) -> TokenReply {
    TokenReply {
      error: Some(error.into()),
      error_description: description.map(str::to_string),
      ..TokenReply::default()
    }
  }

  fn fast(cap_ms: u64) -> PollTiming {
    PollTiming {
      interval: Duration::from_millis(5),
      slow_down_step: Duration::from_millis(5),
      cap: Duration::from_millis(cap_ms),
    }
  }

  #[test]
  fn env_client_id_and_secret_take_precedence_over_the_config() {
    let pair = client_credentials_with(
      Some("env-id".into()),
      Some("env-secret".into()),
      Some("config-id".into()),
      Some("config-secret".into()),
    )
    .unwrap();
    assert_eq!(pair.id, "env-id");
    assert_eq!(pair.secret, "env-secret");

    let pair = client_credentials_with(
      Some(" ".into()),
      None,
      Some("config-id".into()),
      Some("config-secret".into()),
    )
    .unwrap();
    assert_eq!(pair.id, "config-id");
    assert_eq!(pair.secret, "config-secret");
  }

  #[test]
  fn a_missing_secret_falls_back_to_the_client_id() {
    let pair = client_credentials_with(None, None, Some("config-id".into()), None).unwrap();
    assert_eq!(pair.secret, "config-id");
    assert!(client_credentials_with(None, Some("secret".into()), None, None).is_none());
  }

  #[test]
  fn saved_credentials_from_another_client_id_require_a_new_login() {
    assert!(usable_credentials(Some(saved("client-id")), &client()).is_ok());
    assert!(usable_credentials(Some(saved("old-id")), &client()).is_err());
    assert!(usable_credentials(None, &client()).is_err());
    let mut incomplete = saved("client-id");
    incomplete.refresh_token.clear();
    assert!(usable_credentials(Some(incomplete), &client()).is_err());
  }

  #[test]
  fn credentials_round_trip_through_the_file_without_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tidal_credentials.yml");
    assert!(read_credentials(&path).is_none());
    write_credentials(&path, &saved("client-id")).unwrap();
    assert_eq!(read_credentials(&path), Some(saved("client-id")));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("secret"), "{text}");
  }

  #[test]
  fn a_token_is_stale_within_the_expiry_slack() {
    let credentials = saved("client-id");
    assert!(credentials.is_fresh_at(1_000 - EXPIRY_SLACK_SECS - 1));
    assert!(!credentials.is_fresh_at(1_000 - EXPIRY_SLACK_SECS));
    assert!(!credentials.is_fresh_at(2_000));
    let mut empty = saved("client-id");
    empty.access_token.clear();
    assert!(!empty.is_fresh_at(0));
  }

  #[test]
  fn a_refresh_reply_without_a_refresh_token_keeps_the_old_one() {
    let mut credentials = saved("client-id");
    credentials.apply(
      Token {
        access_token: "new".into(),
        refresh_token: None,
        token_type: None,
        expires_in: 3_600,
      },
      100,
    );
    assert_eq!(credentials.access_token, "new");
    assert_eq!(credentials.refresh_token, "refresh");
    assert_eq!(credentials.token_type, "Bearer");
    assert_eq!(credentials.expires_at, 3_700);
  }

  #[test]
  fn token_errors_map_to_the_rfc_8628_outcomes() {
    assert_eq!(
      classify(reply("authorization_pending", None)),
      Err(TokenError::Pending)
    );
    assert_eq!(
      classify(reply("slow_down", None)),
      Err(TokenError::SlowDown)
    );
    assert_eq!(
      classify(reply("invalid_grant", Some("revoked"))),
      Err(TokenError::Revoked("invalid_grant: revoked".into()))
    );
    assert!(matches!(
      classify(reply("expired_token", None)),
      Err(TokenError::Revoked(_))
    ));
    assert_eq!(
      classify(reply("invalid_client", None)),
      Err(TokenError::Failed("invalid_client".into()))
    );
    assert!(matches!(
      classify(TokenReply::default()),
      Err(TokenError::Failed(_))
    ));
  }

  #[test]
  fn a_revoked_grant_asks_for_a_new_login() {
    assert!(needs_login(
      &TokenError::Revoked("invalid_grant".into()).into_anyhow()
    ));
    assert!(!needs_login(
      &TokenError::Failed("invalid_client".into()).into_anyhow()
    ));
  }

  #[test]
  fn poll_timing_defaults_to_five_seconds_and_caps_at_five_minutes() {
    let timing = PollTiming::from_server(0, 0);
    assert_eq!(timing.interval, Duration::from_secs(5));
    assert_eq!(timing.cap, LOGIN_CAP);
    let timing = PollTiming::from_server(2, 3_600);
    assert_eq!(timing.interval, Duration::from_secs(2));
    assert_eq!(timing.cap, LOGIN_CAP);
    assert_eq!(PollTiming::from_server(2, 60).cap, Duration::from_secs(60));
  }

  #[tokio::test]
  async fn the_poll_loop_waits_through_pending_and_slow_down() {
    let pending = r#"{"error":"authorization_pending"}"#;
    let (base, server) = serve(vec![
      Reply::new("400 Bad Request", pending),
      Reply::new("400 Bad Request", r#"{"error":"slow_down"}"#),
      Reply::new("400 Bad Request", pending),
      Reply::new(
        "200 OK",
        r#"{"access_token":"a","refresh_token":"r","token_type":"Bearer","expires_in":600}"#,
      ),
    ])
    .await;
    let token = poll_device_token(
      &Client::new(),
      &AuthEndpoints::at(base),
      &client(),
      "device",
      fast(5_000),
    )
    .await
    .unwrap();
    assert_eq!(token.access_token, "a");
    assert_eq!(token.refresh_token.as_deref(), Some("r"));
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[0].starts_with("POST /token "), "{}", requests[0]);
    assert!(
      requests[0].contains("device_code=device"),
      "{}",
      requests[0]
    );
    assert!(requests[0].contains("client_secret=client-secret"));
  }

  #[tokio::test]
  async fn the_poll_loop_gives_up_at_the_cap() {
    let pending = r#"{"error":"authorization_pending"}"#;
    let (base, _server) = serve(vec![Reply::new("400 Bad Request", pending); 50]).await;
    let err = poll_device_token(
      &Client::new(),
      &AuthEndpoints::at(base),
      &client(),
      "device",
      fast(30),
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("expired"), "{err}");
  }

  #[tokio::test]
  async fn the_device_login_url_gets_a_scheme() {
    let (base, server) = serve(vec![Reply::new(
      "200 OK",
      r#"{"deviceCode":"dc","userCode":"ABCDE","verificationUriComplete":"link.tidal.com/ABCDE","expiresIn":300,"interval":2}"#,
    )])
    .await;
    let login = DeviceLogin::start_at(Client::new(), AuthEndpoints::at(base), client())
      .await
      .unwrap();
    assert_eq!(login.url(), "https://link.tidal.com/ABCDE");
    assert_eq!(login.timing.interval, Duration::from_secs(2));
    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("POST /device_authorization "));
    assert!(requests[0].contains("client_id=client-id"));
    assert!(!requests[0].contains("client-secret"));
  }

  #[tokio::test]
  async fn a_device_login_without_a_code_reports_the_client_id() {
    let (base, _server) = serve(vec![Reply::new(
      "401 Unauthorized",
      r#"{"status":401,"error":"invalid_client"}"#,
    )])
    .await;
    let err = DeviceLogin::start_at(Client::new(), AuthEndpoints::at(base), client())
      .await
      .err()
      .unwrap();
    assert!(err.to_string().contains("client ID"), "{err}");
  }

  /// Runs the real device login against Tidal. Needs a client ID in
  /// `SPOTATUI_TIDAL_CLIENT_ID` (and optionally the secret); open the printed
  /// URL and approve within five minutes. Writes nothing to disk.
  ///
  /// `cargo test --features tidal -- --ignored live_tidal_login --nocapture`
  #[tokio::test]
  #[ignore]
  async fn live_tidal_login() {
    let client = client_credentials_with(
      std::env::var(CLIENT_ID_ENV).ok(),
      std::env::var(CLIENT_SECRET_ENV).ok(),
      None,
      None,
    )
    .expect("set SPOTATUI_TIDAL_CLIENT_ID");
    let login = DeviceLogin::start(client.clone()).await.unwrap();
    println!("Open {} and approve the device", login.url());
    let token = login.wait().await.unwrap();
    let credentials = TidalCredentials::from_token(&client.id, token, unix_now());
    let session = super::super::client::TidalClient::new(client, credentials, None);
    session.load_session().await.unwrap();
    let credentials = session.credentials().await;
    println!(
      "Logged in: user {} in {}",
      credentials.user_id, credentials.country_code
    );
    assert!(!credentials.user_id.is_empty());
    assert!(!credentials.country_code.is_empty());
  }
}
