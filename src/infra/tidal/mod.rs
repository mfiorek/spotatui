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
pub mod dash;
pub mod dispatch;
pub mod manifest;
mod playlist_sync;
#[cfg(test)]
mod probe;
pub mod segments;
pub mod stream;
mod types;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::de::{DeserializeOwned, IgnoredAny};

use crate::core::plugin_api::{ArtistRef, PlaylistInfo, SearchResults, TrackInfo};
use crate::core::source::{MediaSource, Searcher};
use crate::infra::audio::LocalPlayer;
use crate::infra::progressive::Completion;
use auth::{ClientCredentials, DeviceLogin, TidalCredentials};
use client::TidalClient;
use manifest::{Delivered, PlaybackInfo, StreamSource};

/// One Tidal playback session: the listing being played and the current
/// track's download. Lives in the private `App::tidal_playback` field.
pub struct TidalPlaybackState {
  pub player: Arc<LocalPlayer>,
  /// Source handle, reused to fetch each track on Next/advance.
  pub source: Arc<TidalSource>,
  /// The playing listing's tracks in order; the playbar reads `tracks[index]`.
  pub tracks: Vec<TrackInfo>,
  pub index: usize,
  /// Set until the track starts to play so the tick never reads the empty
  /// sink as end-of-track.
  pub advancing: bool,
  /// The current track's file, filled while it plays; `None` until playback
  /// starts.
  pub tempfile: Option<tempfile::NamedTempFile>,
  /// Whether `tempfile` holds every byte; a replay reads it only then.
  pub complete: Option<Completion>,
  /// The delivered format of the current track; `None` until playback starts.
  pub quality: Option<Delivered>,
  /// Backup of the pre-shuffle order while shuffle is on.
  pub shuffle_backup: Option<crate::infra::queue::ShuffleBackup>,
  /// Stamp of the fetch in flight; a finished fetch with another stamp is dropped.
  pub fetch_id: u64,
  /// A seek and pause to apply when the next track is staged (device
  /// recovery, a replay that fetches again, the native queue's resume,
  /// session restore).
  pub resume_at: Option<ResumePoint>,
  /// The fetch task in flight; aborted when the session is replaced or restamped.
  pub fetch: Option<tokio::task::AbortHandle>,
}

impl Drop for TidalPlaybackState {
  fn drop(&mut self) {
    if let Some(fetch) = self.fetch.take() {
      fetch.abort();
    }
  }
}

impl TidalPlaybackState {
  /// The currently playing track, if `index` is in range.
  pub fn current(&self) -> Option<&TrackInfo> {
    self.tracks.get(self.index)
  }

  /// Whether the current track's file is whole, so a replay can read it.
  pub fn file_is_complete(&self) -> bool {
    self.tempfile.is_some() && self.complete.as_ref().is_some_and(Completion::is_complete)
  }

  /// Turn in-place shuffle on or off (see `infra::queue::toggle_shuffle`).
  pub fn set_shuffle(&mut self, on: bool) {
    crate::infra::queue::toggle_shuffle(
      &mut self.tracks,
      &mut self.index,
      &mut self.shuffle_backup,
      on,
    );
  }
}

pub use crate::infra::queue::ResumePoint;

const TRACK_PREFIX: &str = "tidal:track:";
const PLAYLIST_PREFIX: &str = "tidal:playlist:";
const ALBUM_PREFIX: &str = "tidal:album:";
const FAVORITES_URI: &str = "tidal:favorites:tracks";

/// Most v1 list endpoints cap a page at 100 items.
const PAGE_LIMIT: usize = 100;
/// `playlistsAndFavoritePlaylists` answers HTTP 400 above 50.
const PLAYLISTS_PAGE_LIMIT: usize = 50;
/// The most items one listing loads, like Qobuz's cap.
const MAX_ITEMS: usize = 10_000;
const SEARCH_LIMIT: usize = 20;

/// Covers are served by uuid, with the dashes as path separators.
const IMAGE_BASE: &str = "https://resources.tidal.com/images";
const IMAGE_SIZE: &str = "320x320";

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

// ---------------------------------------------------------------------------
// Library and search
// ---------------------------------------------------------------------------

/// A sidebar row's listing, parsed from its URI.
#[derive(Debug, PartialEq)]
enum Listing {
  Favorites,
  Playlist(String),
  Album(String),
}

/// The library and catalog of one login.
pub struct TidalSource {
  client: Arc<TidalClient>,
  /// [`MAX_ITEMS`], lowered by the tests.
  max_items: usize,
}

impl TidalSource {
  pub fn new(client: Arc<TidalClient>) -> Self {
    TidalSource {
      client,
      max_items: MAX_ITEMS,
    }
  }

  /// Follow limit/offset pagination. The next offset is the number of items
  /// received so far, so a short page mid-list skips nothing. Stops on an
  /// empty page, at the reported total, on a short page without a total, or
  /// at `max_items`.
  async fn fetch_list<T: DeserializeOwned>(&self, path: &str, per_page: usize) -> Result<Vec<T>> {
    let mut out: Vec<T> = Vec::new();
    loop {
      let page: types::List<T> = self
        .client
        .get_json(
          path,
          &[
            ("limit", per_page.to_string()),
            ("offset", out.len().to_string()),
          ],
        )
        .await?;
      let got = page.items.len();
      let total = page.total_number_of_items;
      out.extend(page.items);
      if out.len() >= self.max_items {
        out.truncate(self.max_items);
        return Ok(out);
      }
      if got == 0 || (total > 0 && out.len() >= total) || (total == 0 && got < per_page) {
        return Ok(out);
      }
    }
  }

  /// `users/<id>/<rest>` for the logged-in user.
  async fn user_path(&self, rest: &str) -> Result<String> {
    let user_id = self.client.user_id().await;
    if user_id.is_empty() {
      return Err(anyhow!("the Tidal session has no user"));
    }
    Ok(format!("users/{user_id}/{rest}"))
  }

  async fn favorite_track_count(&self) -> Result<u32> {
    let path = self.user_path("favorites/tracks").await?;
    let page: types::List<IgnoredAny> = self
      .client
      .get_json(
        &path,
        &[("limit", "1".to_string()), ("offset", "0".to_string())],
      )
      .await?;
    Ok(page.total_number_of_items as u32)
  }

  /// The user's own playlists and the ones they follow.
  async fn user_playlists(&self) -> Result<Vec<types::Playlist>> {
    let path = self.user_path("playlistsAndFavoritePlaylists").await?;
    let items: Vec<types::PlaylistItem> = self.fetch_list(&path, PLAYLISTS_PAGE_LIMIT).await?;
    Ok(items.into_iter().map(|i| i.playlist).collect())
  }

  async fn favorite_albums(&self) -> Result<Vec<types::Album>> {
    let path = self.user_path("favorites/albums").await?;
    let items: Vec<types::FavoriteItem<types::Album>> = self.fetch_list(&path, PAGE_LIMIT).await?;
    Ok(items.into_iter().map(|i| i.item).collect())
  }

  async fn listing_tracks(&self, listing: &Listing) -> Result<Vec<TrackInfo>> {
    match listing {
      Listing::Favorites => {
        let path = self.user_path("favorites/tracks").await?;
        let items: Vec<types::FavoriteItem<types::Track>> =
          self.fetch_list(&path, PAGE_LIMIT).await?;
        Ok(
          items
            .iter()
            .map(|i| track_to_track_info(&i.item, None))
            .collect(),
        )
      }
      Listing::Playlist(uuid) => {
        let tracks: Vec<types::Track> = self
          .fetch_list(&format!("playlists/{uuid}/tracks"), PAGE_LIMIT)
          .await?;
        Ok(
          tracks
            .iter()
            .map(|t| track_to_track_info(t, None))
            .collect(),
        )
      }
      Listing::Album(id) => {
        // The album is the fallback for tracks listed without one.
        let album_path = format!("albums/{id}");
        let tracks_path = format!("albums/{id}/tracks");
        let (album, tracks) = tokio::try_join!(
          self.client.get_json::<types::Album>(&album_path, &[]),
          self.fetch_list::<types::Track>(&tracks_path, PAGE_LIMIT)
        )?;
        Ok(
          tracks
            .iter()
            .map(|t| track_to_track_info(t, Some(&album)))
            .collect(),
        )
      }
    }
  }
}

impl TidalSource {
  /// Ask for a track's stream at `quality` and decode its manifest.
  pub async fn stream_source(&self, track_id: &str, quality: &str) -> Result<StreamSource> {
    let info: PlaybackInfo = self
      .client
      .get_json(
        &manifest::playback_info_path(track_id),
        &manifest::playback_info_params(quality),
      )
      .await?;
    manifest::stream_source(&info)
  }
}

impl MediaSource for TidalSource {
  fn name(&self) -> &str {
    "Tidal"
  }

  fn scheme(&self) -> &str {
    "tidal"
  }

  /// Favorite tracks, then the user's own and followed playlists, then
  /// favorite albums.
  async fn playlists(&self) -> Result<Vec<PlaylistInfo>> {
    let (favorite_count, playlists, albums) = tokio::try_join!(
      self.favorite_track_count(),
      self.user_playlists(),
      self.favorite_albums()
    )?;
    let mut out = vec![favorites_playlist(favorite_count)];
    out.extend(playlists.iter().map(playlist_to_playlist_info));
    out.extend(albums.iter().map(album_to_playlist_info));
    Ok(out)
  }

  async fn tracks(&self, playlist_uri: &str) -> Result<Vec<TrackInfo>> {
    let listing = listing_from_uri(playlist_uri)?;
    self.listing_tracks(&listing).await
  }
}

impl Searcher for TidalSource {
  /// Tracks only: album and artist rows would route to Spotify-bound events.
  async fn search(&self, query: &str) -> Result<SearchResults> {
    let found: types::List<types::Track> = self
      .client
      .get_json(
        "search/tracks",
        &[
          ("query", query.to_string()),
          ("limit", SEARCH_LIMIT.to_string()),
        ],
      )
      .await?;
    Ok(SearchResults {
      tracks: found
        .items
        .iter()
        .map(|t| track_to_track_info(t, None))
        .collect(),
      albums: vec![],
      artists: vec![],
      playlists: vec![],
      shows: vec![],
    })
  }
}

// ---------------------------------------------------------------------------
// URIs and domain type conversions
// ---------------------------------------------------------------------------

/// Ids go into the request path unescaped, so only uuid characters pass.
fn is_path_safe(id: &str) -> bool {
  !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The numeric id of a `tidal:track:<id>` URI.
pub fn track_id_from_uri(uri: &str) -> Result<&str> {
  uri
    .strip_prefix(TRACK_PREFIX)
    .filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()))
    .ok_or_else(|| anyhow!("Not a tidal track URI: {uri}"))
}

fn listing_from_uri(uri: &str) -> Result<Listing> {
  let listing = if uri == FAVORITES_URI {
    Some(Listing::Favorites)
  } else if let Some(uuid) = uri.strip_prefix(PLAYLIST_PREFIX) {
    is_path_safe(uuid).then(|| Listing::Playlist(uuid.to_string()))
  } else if let Some(id) = uri.strip_prefix(ALBUM_PREFIX) {
    is_path_safe(id).then(|| Listing::Album(id.to_string()))
  } else {
    None
  };
  listing.ok_or_else(|| anyhow!("Not a tidal playlist URI: {uri}"))
}

/// The CDN URL of a cover uuid, or `None` for a missing or empty one.
fn image_url(uuid: Option<&str>) -> Option<String> {
  let uuid = uuid.filter(|u| !u.is_empty())?;
  Some(format!(
    "{IMAGE_BASE}/{}/{IMAGE_SIZE}.jpg",
    uuid.replace('-', "/")
  ))
}

fn favorites_playlist(track_count: u32) -> PlaylistInfo {
  PlaylistInfo {
    uri: FAVORITES_URI.to_string(),
    name: "Favorite tracks".to_string(),
    owner: String::new(),
    track_count,
    id: None,
    owner_id: None,
    collaborative: false,
    public: Some(false),
    image_url: None,
  }
}

fn playlist_to_playlist_info(p: &types::Playlist) -> PlaylistInfo {
  let creator = p.creator.as_ref();
  PlaylistInfo {
    uri: format!("{PLAYLIST_PREFIX}{}", p.uuid),
    name: p.title.clone(),
    owner: creator.and_then(|c| c.name.clone()).unwrap_or_default(),
    track_count: p.number_of_tracks,
    id: Some(p.uuid.clone()),
    // Editorial playlists name creator 0.
    owner_id: creator
      .map(|c| c.id.clone())
      .filter(|id| !id.is_empty() && id != "0"),
    collaborative: false,
    public: p.public_playlist,
    image_url: image_url(p.square_image.as_deref()).or_else(|| image_url(p.image.as_deref())),
  }
}

fn album_to_playlist_info(a: &types::Album) -> PlaylistInfo {
  let artist = a.artist.as_ref().map(|n| n.name.as_str()).unwrap_or("");
  PlaylistInfo {
    uri: format!("{ALBUM_PREFIX}{}", a.id),
    name: if artist.is_empty() {
      a.title.clone()
    } else {
      format!("{} - {artist}", a.title)
    },
    owner: artist.to_string(),
    track_count: a.number_of_tracks,
    id: Some(a.id.clone()),
    owner_id: None,
    collaborative: false,
    public: Some(true),
    image_url: image_url(a.cover.as_deref()),
  }
}

fn artist_ref(a: &types::Artist) -> ArtistRef {
  ArtistRef {
    id: Some(a.id.clone()).filter(|id| !id.is_empty()),
    name: a.name.clone(),
  }
}

/// Map a Tidal track onto [`TrackInfo`]; `fallback` is the album of an
/// `albums/<id>/tracks` listing. The artists are the track's full list, else
/// its main artist, else the album's.
fn track_to_track_info(t: &types::Track, fallback: Option<&types::Album>) -> TrackInfo {
  let album = t.album.as_ref().or(fallback);
  let mut artist_refs: Vec<ArtistRef> = t
    .artists
    .iter()
    .filter(|a| !a.name.is_empty())
    .map(artist_ref)
    .collect();
  if artist_refs.is_empty() {
    artist_refs = t
      .artist
      .as_ref()
      .or(album.and_then(|a| a.artist.as_ref()))
      .filter(|a| !a.name.is_empty())
      .map(|a| vec![artist_ref(a)])
      .unwrap_or_default();
  }
  let name = match t.version.as_deref().filter(|v| !v.is_empty()) {
    Some(version) => format!("{} ({version})", t.title),
    None => t.title.clone(),
  };
  TrackInfo {
    uri: Some(format!("{TRACK_PREFIX}{}", t.id)),
    name,
    artists: artist_refs.iter().map(|a| a.name.clone()).collect(),
    album: album.map(|a| a.title.clone()).unwrap_or_default(),
    duration_ms: t.duration * 1000,
    id: Some(t.id.clone()),
    album_id: album.map(|a| a.id.clone()).filter(|id| !id.is_empty()),
    artist_refs,
    is_playable: t.allow_streaming && t.stream_ready,
    is_local: false,
    track_number: t.track_number,
    explicit: t.explicit,
    // A nested listing's album often has no cover; the fallback has it.
    image_url: image_url(album.and_then(|a| a.cover.as_deref()))
      .or_else(|| image_url(fallback.and_then(|a| a.cover.as_deref()))),
  }
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
    /// Extra header lines, each `name: value`.
    headers: Vec<String>,
  }

  impl Reply {
    pub fn new(status: &'static str, body: impl Into<String>) -> Self {
      Reply {
        status,
        body: body.into(),
        headers: Vec::new(),
      }
    }

    /// The reply with one more header.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
      self.headers.push(format!("{name}: {value}"));
      self
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
        let extra: String = reply.headers.iter().map(|h| format!("{h}\r\n")).collect();
        let response = format!(
          "HTTP/1.1 {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n{}",
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

  /// A source logged in as user 42 against `base`.
  pub fn source_at(base: &str) -> super::TidalSource {
    use super::auth::{self, ClientCredentials, TidalCredentials};
    let client = ClientCredentials {
      id: "client-id".into(),
      secret: "client-secret".into(),
    };
    let credentials = TidalCredentials {
      client_id: "client-id".into(),
      access_token: "token".into(),
      refresh_token: "refresh".into(),
      token_type: "Bearer".into(),
      expires_at: auth::unix_now() + 3_600,
      user_id: "42".into(),
      country_code: "NO".into(),
    };
    super::TidalSource::new(std::sync::Arc::new(super::TidalClient::at(
      base,
      client,
      credentials,
    )))
  }

  /// A file server that honours `Range`, for the download tests.
  pub struct FileServer {
    requests: std::sync::Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>,
  }

  impl FileServer {
    /// The `Range` header of each request so far, in arrival order.
    pub fn ranges(&self) -> Vec<Option<String>> {
      self
        .requests()
        .into_iter()
        .map(|(_, range)| range)
        .collect()
    }

    /// The path and `Range` header of each request so far, in arrival order.
    pub fn requests(&self) -> Vec<(String, Option<String>)> {
      self.requests.lock().unwrap().clone()
    }
  }

  /// Serve `body` at `<base>/track.m4a`, one connection per request, in 16 KiB
  /// chunks with a pause between them so a download is still running when
  /// the reader seeks. Request number `hang_from` and later get their headers
  /// and then no body.
  pub async fn serve_file(body: Vec<u8>, hang_from: Option<usize>) -> (String, FileServer) {
    let (base, server) = serve_files(vec![("/track.m4a".to_string(), body)], hang_from, true).await;
    (format!("{base}/track.m4a"), server)
  }

  /// Serve each `(path, body)` like [`serve_file`]; any other path is a 404.
  /// Without `honour_range`, every request gets the whole body with a 200.
  /// Returns the base URL.
  pub async fn serve_files(
    files: Vec<(String, Vec<u8>)>,
    hang_from: Option<usize>,
    honour_range: bool,
  ) -> (String, FileServer) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = std::sync::Arc::clone(&requests);
    let files: std::sync::Arc<std::collections::HashMap<String, Vec<u8>>> =
      std::sync::Arc::new(files.into_iter().collect());
    tokio::spawn(async move {
      while let Ok((mut stream, _)) = listener.accept().await {
        let files = std::sync::Arc::clone(&files);
        let log = std::sync::Arc::clone(&log);
        tokio::spawn(async move {
          let (read_half, mut write_half) = stream.split();
          let mut reader = BufReader::new(read_half);
          let mut path = String::new();
          let mut range = None;
          loop {
            let mut line = String::new();
            if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line == "\r\n" {
              break;
            }
            if path.is_empty() {
              let target = line.split_whitespace().nth(1).unwrap_or("");
              path = target.split('?').next().unwrap_or("").to_string();
            } else if let Some((name, value)) = line.split_once(':') {
              if name.eq_ignore_ascii_case("range") {
                range = Some(value.trim().to_string());
              }
            }
          }
          let number = {
            let mut log = log.lock().unwrap();
            log.push((path.clone(), range.clone()));
            log.len() - 1
          };
          let Some(body) = files.get(&path) else {
            let _ = write_half
              .write_all(
                b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
              )
              .await;
            return;
          };
          let range = range.filter(|_| honour_range);
          let total = body.len();
          let (start, end) = range
            .as_deref()
            .and_then(|r| r.strip_prefix("bytes="))
            .and_then(|r| r.split_once('-'))
            .map(|(start, end)| {
              let start: usize = start.parse().unwrap_or(0);
              let end: usize = end.parse().map_or(total, |e: usize| e + 1);
              (start.min(total), end.min(total))
            })
            .unwrap_or((0, total));
          let head = if range.is_some() {
            format!(
              "HTTP/1.1 206 Partial Content\r\ncontent-type: audio/mp4\r\naccept-ranges: bytes\r\ncontent-range: bytes {start}-{}/{total}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
              end.saturating_sub(1),
              end - start
            )
          } else {
            format!(
              "HTTP/1.1 200 OK\r\ncontent-type: audio/mp4\r\naccept-ranges: bytes\r\ncontent-length: {total}\r\nconnection: close\r\n\r\n"
            )
          };
          if write_half.write_all(head.as_bytes()).await.is_err() {
            return;
          }
          if hang_from.is_some_and(|n| number >= n) {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            return;
          }
          for chunk in body[start..end].chunks(16 * 1024) {
            if write_half.write_all(chunk).await.is_err() {
              return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
          }
          let _ = write_half.flush().await;
        });
      }
    });
    (base, FileServer { requests })
  }
}

#[cfg(test)]
mod tests {
  use base64::Engine as _;

  use super::test_server::{serve, source_at, Reply};
  use super::*;

  fn track(json: &str) -> types::Track {
    serde_json::from_str(json).unwrap()
  }

  fn page(ids: &[u32], total: usize) -> Reply {
    let items: Vec<String> = ids
      .iter()
      .map(|id| format!(r#"{{"id":{id},"title":"T{id}"}}"#))
      .collect();
    Reply::new(
      "200 OK",
      format!(
        r#"{{"items":[{}],"totalNumberOfItems":{total}}}"#,
        items.join(",")
      ),
    )
  }

  #[test]
  fn sidebar_uris_parse_into_listings() {
    assert_eq!(
      listing_from_uri("tidal:favorites:tracks").unwrap(),
      Listing::Favorites
    );
    assert_eq!(
      listing_from_uri("tidal:playlist:36ea71a8-445e-41a4-82ab-6628c581535d").unwrap(),
      Listing::Playlist("36ea71a8-445e-41a4-82ab-6628c581535d".into())
    );
    assert_eq!(
      listing_from_uri("tidal:album:77646169").unwrap(),
      Listing::Album("77646169".into())
    );
  }

  #[test]
  fn foreign_or_unsafe_uris_are_not_listings() {
    for uri in [
      "tidal:track:1",
      "qobuz:album:1",
      "tidal:album:",
      "tidal:playlist:../sessions",
      "tidal:album:1?limit=1",
    ] {
      assert!(listing_from_uri(uri).is_err(), "{uri}");
    }
  }

  #[test]
  fn a_cover_uuid_becomes_a_cdn_path() {
    assert_eq!(
      image_url(Some("ab12-cd34-ef56")).as_deref(),
      Some("https://resources.tidal.com/images/ab12/cd34/ef56/320x320.jpg")
    );
    assert_eq!(image_url(Some("")), None);
    assert_eq!(image_url(None), None);
  }

  #[test]
  fn a_track_maps_every_artist_and_its_album() {
    let t = track(
      r#"{"id":11,"title":"Song","version":"Remastered","trackNumber":3,"duration":200,
          "explicit":true,"allowStreaming":true,"streamReady":true,
          "artist":{"id":1,"name":"Main"},
          "artists":[{"id":1,"name":"Main"},{"id":2,"name":"Guest"}],
          "album":{"id":5,"title":"Record","cover":"aa-bb"}}"#,
    );
    let info = track_to_track_info(&t, None);
    assert_eq!(info.uri.as_deref(), Some("tidal:track:11"));
    assert_eq!(info.name, "Song (Remastered)");
    assert_eq!(info.artists, vec!["Main", "Guest"]);
    assert_eq!(info.artist_refs[1].id.as_deref(), Some("2"));
    assert_eq!(info.album, "Record");
    assert_eq!(info.album_id.as_deref(), Some("5"));
    assert_eq!(info.duration_ms, 200_000);
    assert_eq!(info.track_number, 3);
    assert!(info.explicit);
    assert!(info.is_playable);
    assert_eq!(
      info.image_url.as_deref(),
      Some("https://resources.tidal.com/images/aa/bb/320x320.jpg")
    );
  }

  #[test]
  fn a_track_without_artists_falls_back_to_the_main_then_the_album_artist() {
    let main = track(r#"{"id":1,"title":"A","artist":{"id":7,"name":"Main"}}"#);
    assert_eq!(track_to_track_info(&main, None).artists, vec!["Main"]);

    let album: types::Album = serde_json::from_str(
      r#"{"id":9,"title":"Rec","cover":"cc-dd","artist":{"id":3,"name":"Band"}}"#,
    )
    .unwrap();
    let bare = track(r#"{"id":2,"title":"B"}"#);
    let info = track_to_track_info(&bare, Some(&album));
    assert_eq!(info.artists, vec!["Band"]);
    assert_eq!(info.album, "Rec");
    assert_eq!(info.album_id.as_deref(), Some("9"));
    assert!(info.image_url.is_some());
  }

  #[test]
  fn a_track_needs_both_streaming_flags_to_be_playable() {
    let not_ready = track(r#"{"id":1,"title":"A","allowStreaming":true,"streamReady":false}"#);
    let not_allowed = track(r#"{"id":1,"title":"A","allowStreaming":false,"streamReady":true}"#);
    assert!(!track_to_track_info(&not_ready, None).is_playable);
    assert!(!track_to_track_info(&not_allowed, None).is_playable);
  }

  #[test]
  fn a_playlist_prefers_the_square_cover_and_hides_an_editorial_creator() {
    let p: types::Playlist = serde_json::from_str(
      r#"{"uuid":"u-1","title":"Mix","numberOfTracks":12,"publicPlaylist":false,
          "creator":{"id":0},"squareImage":"sq-1","image":"wide-1"}"#,
    )
    .unwrap();
    let info = playlist_to_playlist_info(&p);
    assert_eq!(info.uri, "tidal:playlist:u-1");
    assert_eq!(info.track_count, 12);
    assert_eq!(info.owner_id, None);
    assert_eq!(info.public, Some(false));
    assert_eq!(
      info.image_url.as_deref(),
      Some("https://resources.tidal.com/images/sq/1/320x320.jpg")
    );

    let wide: types::Playlist =
      serde_json::from_str(r#"{"uuid":"u-2","title":"Mine","creator":{"id":42},"image":"wide-1"}"#)
        .unwrap();
    let info = playlist_to_playlist_info(&wide);
    assert_eq!(info.owner_id.as_deref(), Some("42"));
    assert_eq!(
      info.image_url.as_deref(),
      Some("https://resources.tidal.com/images/wide/1/320x320.jpg")
    );
  }

  #[test]
  fn an_album_row_names_its_artist() {
    let a: types::Album = serde_json::from_str(
      r#"{"id":"5","title":"Record","numberOfTracks":9,"artist":{"id":3,"name":"Band"}}"#,
    )
    .unwrap();
    let info = album_to_playlist_info(&a);
    assert_eq!(info.uri, "tidal:album:5");
    assert_eq!(info.name, "Record - Band");
    assert_eq!(info.track_count, 9);
  }

  #[tokio::test]
  async fn pagination_advances_by_items_received_until_the_total() {
    let (base, server) = serve(vec![page(&[1, 2], 3), page(&[3], 3)]).await;
    let items: Vec<types::Track> = source_at(&base).fetch_list("things", 2).await.unwrap();
    assert_eq!(items.len(), 3);
    let requests = server.await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].starts_with("GET /v1/things?limit=2&offset=0&countryCode=NO "));
    assert!(requests[1].contains("offset=2"), "{}", requests[1]);
  }

  #[tokio::test]
  async fn a_short_page_without_a_total_ends_the_list() {
    let (base, server) = serve(vec![page(&[1, 2], 0), page(&[3], 0)]).await;
    let items: Vec<types::Track> = source_at(&base).fetch_list("things", 2).await.unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(server.await.unwrap().len(), 2);
  }

  #[tokio::test]
  async fn an_empty_page_ends_the_list_before_the_total() {
    let (base, server) = serve(vec![page(&[1, 2], 10), page(&[], 10)]).await;
    let items: Vec<types::Track> = source_at(&base).fetch_list("things", 2).await.unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(server.await.unwrap().len(), 2);
  }

  #[tokio::test]
  async fn pagination_stops_at_the_item_cap() {
    let (base, server) = serve(vec![page(&[1, 2], 10), page(&[3, 4], 10)]).await;
    let source = TidalSource {
      max_items: 3,
      ..source_at(&base)
    };
    let items: Vec<types::Track> = source.fetch_list("things", 2).await.unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(server.await.unwrap().len(), 2);
  }

  #[tokio::test]
  async fn playlists_page_at_fifty_under_the_user() {
    let (base, server) = serve(vec![Reply::new(
      "200 OK",
      r#"{"items":[{"type":"USER_CREATED","playlist":{"uuid":"u-1","title":"Mix"}}],"totalNumberOfItems":1}"#,
    )])
    .await;
    let playlists = source_at(&base).user_playlists().await.unwrap();
    assert_eq!(playlists[0].uuid, "u-1");
    let requests = server.await.unwrap();
    assert!(
      requests[0].starts_with("GET /v1/users/42/playlistsAndFavoritePlaylists?limit=50&offset=0"),
      "{}",
      requests[0]
    );
  }

  #[tokio::test]
  async fn favorite_tracks_unwrap_their_items() {
    let (base, server) = serve(vec![Reply::new(
      "200 OK",
      r#"{"items":[{"created":"2024-01-01","item":{"id":11,"title":"Song"}}],"totalNumberOfItems":1}"#,
    )])
    .await;
    let tracks = source_at(&base)
      .tracks("tidal:favorites:tracks")
      .await
      .unwrap();
    assert_eq!(tracks[0].uri.as_deref(), Some("tidal:track:11"));
    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("GET /v1/users/42/favorites/tracks?limit=100&offset=0"));
  }

  #[tokio::test]
  async fn search_asks_for_tracks_only() {
    let (base, server) = serve(vec![page(&[1], 1)]).await;
    let results = source_at(&base).search("daft punk").await.unwrap();
    assert_eq!(results.tracks.len(), 1);
    assert!(results.albums.is_empty());
    let requests = server.await.unwrap();
    assert!(
      requests[0].starts_with("GET /v1/search/tracks?query=daft+punk&limit=20"),
      "{}",
      requests[0]
    );
  }

  #[tokio::test]
  async fn a_stream_is_asked_for_with_the_full_asset_and_decoded() {
    let manifest = base64::engine::general_purpose::STANDARD
      .encode(r#"{"mimeType":"audio/mp4","encryptionType":"NONE","urls":["https://cdn/t.m4a"]}"#);
    let reply = format!(
      r#"{{"audioQuality":"HIGH","manifestMimeType":"application/vnd.tidal.bts","manifest":"{manifest}"}}"#
    );
    let (base, server) = serve(vec![Reply::new("200 OK", reply)]).await;
    let stream = source_at(&base)
      .stream_source("77", manifest::HI_RES_QUALITY)
      .await
      .unwrap();
    assert!(
      matches!(&stream.kind, manifest::StreamKind::Bts { url, .. } if url == "https://cdn/t.m4a"),
      "{stream:?}"
    );
    assert_eq!(stream.delivered, Delivered::High);
    let requests = server.await.unwrap();
    assert!(
      requests[0].starts_with(
        "GET /v1/tracks/77/playbackinfopostpaywall?playbackmode=STREAM&audioquality=HI_RES_LOSSLESS&assetpresentation=FULL&countryCode=NO"
      ),
      "{}",
      requests[0]
    );
  }

  #[test]
  fn track_uris_admit_numeric_ids_only() {
    assert_eq!(track_id_from_uri("tidal:track:123").unwrap(), "123");
    for uri in [
      "tidal:track:",
      "tidal:track:1/2",
      "tidal:album:1",
      "qobuz:track:1",
    ] {
      assert!(track_id_from_uri(uri).is_err(), "{uri}");
    }
  }

  /// Browses the real library with the login the app saved. The client ID
  /// comes from `config.yml` or `SPOTATUI_TIDAL_CLIENT_ID`, as in the app; a
  /// rotated token is saved like the app does.
  ///
  /// `cargo test --features tidal -- --ignored live_tidal_browse --nocapture`
  #[tokio::test]
  #[ignore = "needs a saved Tidal login, a client ID and the network"]
  async fn live_tidal_browse() {
    let mut config = crate::core::user_config::UserConfig::new();
    config.load_config().expect("config.yml");
    let client = auth::client_credentials(&config.behavior).expect("a Tidal client ID");
    let source = TidalSource::new(restore_login(client).await.expect("a saved login"));

    let rows = source.playlists().await.expect("sidebar rows");
    println!("{} sidebar rows", rows.len());
    for row in rows.iter().take(5) {
      println!("  {} ({} tracks) {}", row.name, row.track_count, row.uri);
    }
    let tracks = source.tracks(&rows[0].uri).await.expect("favorite tracks");
    println!("{}: {} tracks", rows[0].name, tracks.len());
    if let Some(album) = rows.iter().find(|r| r.uri.starts_with(ALBUM_PREFIX)) {
      let tracks = source.tracks(&album.uri).await.expect("album tracks");
      println!("{}: {} tracks", album.name, tracks.len());
    }
    let results = source.search("Daft Punk").await.expect("search");
    let hit = results.tracks.first().expect("a search hit");
    println!("search: {} - {}", hit.name, hit.artists.join(", "));
  }

  /// Opens a hi-res track and one without a hi-res master through the
  /// playback path: the manifest, the download into a tempfile (DASH
  /// segments or the BTS file) and a decoder over it, which probes the
  /// container. Then asks for the hi-res track at LOW, which must come back
  /// as AAC 96. No audio device needed.
  ///
  /// `cargo test --features tidal -- --ignored live_tidal_stream --nocapture`
  #[tokio::test(flavor = "multi_thread")]
  #[ignore = "needs a saved Tidal login, a client ID and the network"]
  async fn live_tidal_stream() {
    let mut config = crate::core::user_config::UserConfig::new();
    config.load_config().expect("config.yml");
    let client = auth::client_credentials(&config.behavior).expect("a Tidal client ID");
    let source = TidalSource::new(restore_login(client).await.expect("a saved login"));

    // Hi-res as of 2026-10, then a track Tidal has no hi-res master of.
    let hi_res = "Taylor Swift The Fate of Ophelia";
    for (query, quality) in [
      (hi_res, manifest::HI_RES_QUALITY),
      ("Daft Punk Get Lucky", manifest::HI_RES_QUALITY),
      (hi_res, "LOW"),
    ] {
      let results = source.search(query).await.expect("search");
      let track = results.tracks.first().expect("a search hit");
      let uri = track.uri.as_deref().expect("a track URI");
      let started = std::time::Instant::now();
      let prepared = dispatch::prepare_track(&source, track_id_from_uri(uri).unwrap(), quality)
        .await
        .expect("a playable stream");
      println!(
        "{} at {quality}: {} in {:?}",
        track.name,
        prepared.delivered.label(),
        started.elapsed()
      );
      if quality == "LOW" {
        assert_eq!(prepared.delivered, Delivered::Low);
      }
    }
  }
}
