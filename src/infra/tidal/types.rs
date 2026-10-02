//! Serde types for the private API responses, private to the module. Every
//! field defaults, so a missing one never fails a whole listing.

use serde::{Deserialize, Deserializer};

/// A Tidal id: integers for tracks, albums and artists, sometimes strings.
fn de_id<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
  #[derive(Deserialize)]
  #[serde(untagged)]
  enum RawId {
    Num(u64),
    Str(String),
    Null(()),
  }
  Ok(match RawId::deserialize(d)? {
    RawId::Num(n) => n.to_string(),
    RawId::Str(s) => s,
    RawId::Null(()) => String::new(),
  })
}

/// One page of a list endpoint: `{ limit, offset, totalNumberOfItems, items }`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct List<T> {
  #[serde(default = "Vec::new")]
  pub items: Vec<T>,
  #[serde(default)]
  pub total_number_of_items: usize,
}

/// The `{ created, item }` wrapper of the `users/<id>/favorites/*` lists.
#[derive(Debug, Deserialize)]
pub struct FavoriteItem<T> {
  pub item: T,
}

/// The `{ type, created, playlist }` wrapper of `playlistsAndFavoritePlaylists`.
#[derive(Debug, Deserialize)]
pub struct PlaylistItem {
  pub playlist: Playlist,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Artist {
  #[serde(default, deserialize_with = "de_id")]
  pub id: String,
  #[serde(default)]
  pub name: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Album {
  #[serde(default, deserialize_with = "de_id")]
  pub id: String,
  #[serde(default)]
  pub title: String,
  #[serde(default)]
  pub number_of_tracks: u32,
  /// The cover image uuid; see [`super::image_url`].
  #[serde(default)]
  pub cover: Option<String>,
  #[serde(default)]
  pub artist: Option<Artist>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Track {
  #[serde(default, deserialize_with = "de_id")]
  pub id: String,
  #[serde(default)]
  pub title: String,
  /// "Remastered", "Live", ...; shown as a suffix like Qobuz's.
  #[serde(default)]
  pub version: Option<String>,
  #[serde(default)]
  pub track_number: u32,
  /// Seconds.
  #[serde(default)]
  pub duration: u64,
  #[serde(default)]
  pub explicit: bool,
  #[serde(default)]
  pub allow_streaming: bool,
  #[serde(default)]
  pub stream_ready: bool,
  #[serde(default)]
  pub artist: Option<Artist>,
  #[serde(default)]
  pub artists: Vec<Artist>,
  /// Absent on some nested listings; the caller supplies a fallback.
  #[serde(default)]
  pub album: Option<Album>,
  /// What playlist sync matches tracks by across sources.
  #[serde(default)]
  pub isrc: Option<String>,
}

/// One row of `playlists/<uuid>/items`: a track or a video. Item indices
/// count both.
#[derive(Debug, Default, Deserialize)]
pub struct PlaylistEntry {
  #[serde(default)]
  pub item: Track,
  #[serde(default, rename = "type")]
  pub kind: String,
}

/// The `{ data }` reply of the v2 create-playlist call.
#[derive(Debug, Deserialize)]
pub struct CreatedPlaylist {
  #[serde(default)]
  pub data: Option<Playlist>,
}

/// The playlist creator; `id` 0 for editorial playlists.
#[derive(Debug, Default, Deserialize)]
pub struct Creator {
  #[serde(default, deserialize_with = "de_id")]
  pub id: String,
  #[serde(default)]
  pub name: Option<String>,
}

/// Playlists are keyed by uuid, not numeric id.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Playlist {
  #[serde(default)]
  pub uuid: String,
  #[serde(default)]
  pub title: String,
  #[serde(default)]
  pub number_of_tracks: u32,
  #[serde(default)]
  pub public_playlist: Option<bool>,
  #[serde(default)]
  pub creator: Option<Creator>,
  /// The square cover uuid; preferred for the sidebar.
  #[serde(default)]
  pub square_image: Option<String>,
  /// The wide cover uuid; the fallback.
  #[serde(default)]
  pub image: Option<String>,
}
