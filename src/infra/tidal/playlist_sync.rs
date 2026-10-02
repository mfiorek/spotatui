//! What playlist sync needs from Tidal: a playlist's items with their ISRCs,
//! catalog search as sync candidates, and the three playlist writes.
//!
//! The writes are the private-API calls python-tidal sends. A playlist's
//! items change only under its current `ETag` (`If-None-Match`), read fresh
//! before every write, so an edit made elsewhere in between is refused rather
//! than overwritten. Creation is a v2 call into the root folder. Item indices
//! count videos too, so a removal reads `playlists/<uuid>/items`, never the
//! track-only listing.

use anyhow::{anyhow, Result};
use reqwest::Method;

use super::client::{Api, Call};
use super::{is_path_safe, listing_from_uri, types, Listing, TidalSource, PAGE_LIMIT};
use super::{PLAYLIST_PREFIX, TRACK_PREFIX};
use crate::core::playlist_sync::SyncTrack;
use crate::core::source::PlaylistWriter;
use crate::infra::playlist_sync::PlaylistRead;

/// Track ids (or item indices) per write call.
const WRITE_CHUNK: usize = 50;

impl TidalSource {
  /// The uri of the user's own playlist named `name`. Followed playlists are
  /// left out: they cannot be written.
  pub(crate) async fn own_playlist_named(&self, name: &str) -> Result<Option<String>> {
    let user_id = self.client.user_id().await;
    Ok(
      self
        .user_playlists()
        .await?
        .iter()
        .find(|p| {
          p.creator.as_ref().is_some_and(|c| c.id == user_id)
            && crate::infra::playlist_sync::same_name(&p.title, name)
        })
        .map(|p| format!("{PLAYLIST_PREFIX}{}", p.uuid)),
    )
  }

  /// Every item of a playlist: tracks as sync candidates, anything else (a
  /// video) as not syncable.
  pub(crate) async fn sync_playlist(&self, playlist_uri: &str) -> Result<PlaylistRead> {
    let uuid = playlist_uuid(playlist_uri)?;
    Ok(split_entries(&self.playlist_entries(&uuid).await?))
  }

  /// Catalog search results as sync candidates, ISRC included.
  pub(crate) async fn sync_search(&self, query: &str, limit: u32) -> Result<Vec<SyncTrack>> {
    let found: types::List<types::Track> = self
      .client
      .get_json(
        "search/tracks",
        &[("query", query.to_string()), ("limit", limit.to_string())],
      )
      .await?;
    Ok(found.items.iter().map(track_to_sync_track).collect())
  }

  /// Create a playlist in the root folder and return its uuid.
  pub(crate) async fn create_playlist(&self, name: &str) -> Result<String> {
    let params = [
      ("name", name.to_string()),
      ("description", String::new()),
      ("folderId", "root".to_string()),
    ];
    let created: types::CreatedPlaylist = self
      .client
      .send_json(&Call {
        api: Api::V2,
        method: Method::PUT,
        path: "my-collection/playlists/folders/create-playlist",
        params: &params,
        form: &[],
        if_none_match: None,
      })
      .await?;
    created
      .data
      .map(|p| p.uuid)
      .filter(|uuid| is_path_safe(uuid))
      .ok_or_else(|| anyhow!("Tidal created no playlist"))
  }

  async fn playlist_entries(&self, uuid: &str) -> Result<Vec<types::PlaylistEntry>> {
    self
      .fetch_list(&format!("playlists/{uuid}/items"), PAGE_LIMIT)
      .await
  }

  /// The playlist's current `ETag` and length, which every write needs.
  async fn playlist_state(&self, uuid: &str) -> Result<(String, u32)> {
    let path = format!("playlists/{uuid}");
    let reply = self.client.send(&Call::get(&path, &[])).await?;
    let playlist: types::Playlist = serde_json::from_str(&reply.body)?;
    let etag = reply
      .etag
      .ok_or_else(|| anyhow!("Tidal sent no ETag for playlist {uuid}"))?;
    Ok((etag, playlist.number_of_tracks))
  }
}

impl PlaylistWriter for TidalSource {
  /// Append tracks, [`WRITE_CHUNK`] ids per call; Tidal skips duplicates.
  async fn add_tracks(&self, playlist_uri: &str, track_uris: &[String]) -> Result<()> {
    let uuid = playlist_uuid(playlist_uri)?;
    let ids = track_ids(track_uris)?;
    let path = format!("playlists/{uuid}/items");
    for chunk in ids.chunks(WRITE_CHUNK) {
      let (etag, length) = self.playlist_state(&uuid).await?;
      let form = [
        ("onArtifactNotFound", "SKIP".to_string()),
        ("trackIds", chunk.join(",")),
        ("toIndex", length.to_string()),
        ("onDupes", "SKIP".to_string()),
      ];
      self
        .client
        .send(&Call {
          method: Method::POST,
          form: &form,
          if_none_match: Some(&etag),
          ..Call::get(&path, &[])
        })
        .await?;
    }
    Ok(())
  }

  /// Remove tracks by item index, the highest indices first so the lower
  /// ones still name the same rows.
  async fn remove_tracks(&self, playlist_uri: &str, track_uris: &[String]) -> Result<()> {
    let uuid = playlist_uuid(playlist_uri)?;
    if track_uris.is_empty() {
      return Ok(());
    }
    let ids = track_ids(track_uris)?;
    let entries = self.playlist_entries(&uuid).await?;
    for chunk in indices_for(&entries, &ids).chunks(WRITE_CHUNK) {
      let (etag, _) = self.playlist_state(&uuid).await?;
      let list: Vec<String> = chunk.iter().map(usize::to_string).collect();
      let path = format!("playlists/{uuid}/items/{}", list.join(","));
      self
        .client
        .send(&Call {
          method: Method::DELETE,
          if_none_match: Some(&etag),
          ..Call::get(&path, &[])
        })
        .await?;
    }
    Ok(())
  }
}

/// The uuid of a `tidal:playlist:` uri; favorites and albums cannot sync.
fn playlist_uuid(uri: &str) -> Result<String> {
  match listing_from_uri(uri)? {
    Listing::Playlist(uuid) => Ok(uuid),
    _ => Err(anyhow!("Not a Tidal playlist: {uri}")),
  }
}

/// Numeric track ids from sync keys, which are bare ids or track uris.
fn track_ids(keys: &[String]) -> Result<Vec<String>> {
  keys
    .iter()
    .map(|key| {
      let id = key.strip_prefix(TRACK_PREFIX).unwrap_or(key);
      if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
        Ok(id.to_string())
      } else {
        Err(anyhow!("Not a Tidal track: {key}"))
      }
    })
    .collect()
}

/// The index of the last occurrence of each wanted track, highest first: the
/// sync adds one row per track, so one row goes and an earlier hand-added
/// copy stays.
fn indices_for(entries: &[types::PlaylistEntry], ids: &[String]) -> Vec<usize> {
  let mut indices: Vec<usize> = ids
    .iter()
    .filter_map(|id| {
      entries
        .iter()
        .rposition(|e| e.kind == "track" && e.item.id == *id)
    })
    .collect();
  indices.sort_unstable_by(|a, b| b.cmp(a));
  indices.dedup();
  indices
}

fn split_entries(entries: &[types::PlaylistEntry]) -> PlaylistRead {
  let mut read = PlaylistRead::default();
  for entry in entries.iter().filter(|e| !e.item.id.is_empty()) {
    let track = track_to_sync_track(&entry.item);
    if entry.kind == "track" {
      read.tracks.push(track);
    } else {
      read.not_syncable.push(track);
    }
  }
  read
}

fn track_to_sync_track(t: &types::Track) -> SyncTrack {
  let artist = t.artist.as_ref().or(t.artists.first());
  SyncTrack {
    key: t.id.clone(),
    isrc: t.isrc.clone().filter(|isrc| !isrc.is_empty()),
    title: t.title.clone(),
    artist: artist.map(|a| a.name.clone()).unwrap_or_default(),
    duration_ms: (t.duration > 0).then_some(t.duration * 1000),
  }
}

#[cfg(test)]
mod tests {
  use super::super::test_server::{serve, source_at, Reply};
  use super::*;

  const PLAYLIST: &str = "tidal:playlist:3f2a-9c";

  fn entry(kind: &str, id: &str) -> types::PlaylistEntry {
    types::PlaylistEntry {
      item: types::Track {
        id: id.to_string(),
        title: format!("T{id}"),
        ..Default::default()
      },
      kind: kind.to_string(),
    }
  }

  fn state(etag: &str, tracks: u32) -> Reply {
    Reply::new(
      "200 OK",
      format!(r#"{{"uuid":"3f2a-9c","numberOfTracks":{tracks}}}"#),
    )
    .with_header("ETag", etag)
  }

  #[test]
  fn a_removal_takes_the_last_copy_of_each_track_highest_index_first() {
    let entries = [
      entry("track", "1"),
      entry("video", "2"),
      entry("track", "2"),
      entry("track", "1"),
    ];
    let ids = ["1".to_string(), "2".to_string(), "9".to_string()];

    assert_eq!(indices_for(&entries, &ids), vec![3, 2]);
  }

  #[test]
  fn a_video_is_listed_as_not_syncable() {
    let read = split_entries(&[entry("track", "1"), entry("video", "2")]);

    assert_eq!(read.keys(), vec!["1".to_string()]);
    assert_eq!(read.not_syncable.len(), 1);
    assert_eq!(read.not_syncable[0].key, "2");
  }

  #[test]
  fn a_sync_track_keeps_the_isrc_and_the_first_artist() {
    let track: types::Track = serde_json::from_str(
      r#"{"id":7,"title":"Song","version":"Live","duration":200,"isrc":"USAB10000001",
          "artists":[{"id":1,"name":"First"},{"id":2,"name":"Second"}]}"#,
    )
    .unwrap();

    assert_eq!(
      track_to_sync_track(&track),
      SyncTrack {
        key: "7".to_string(),
        isrc: Some("USAB10000001".to_string()),
        title: "Song".to_string(),
        artist: "First".to_string(),
        duration_ms: Some(200_000),
      }
    );
  }

  #[test]
  fn keys_are_bare_ids_or_track_uris_and_nothing_else() {
    let keys = ["12".to_string(), "tidal:track:34".to_string()];
    assert_eq!(track_ids(&keys).unwrap(), vec!["12", "34"]);
    assert!(track_ids(&["tidal:album:1".to_string()]).is_err());
    assert!(track_ids(&["1/../x".to_string()]).is_err());
  }

  #[test]
  fn only_a_playlist_uri_can_sync() {
    assert_eq!(playlist_uuid(PLAYLIST).unwrap(), "3f2a-9c");
    assert!(playlist_uuid("tidal:favorites:tracks").is_err());
    assert!(playlist_uuid("tidal:album:5").is_err());
  }

  #[tokio::test]
  async fn adding_appends_under_the_fresh_etag() {
    let (base, server) = serve(vec![state("\"v7\"", 3), Reply::new("200 OK", "{}")]).await;

    source_at(&base)
      .add_tracks(PLAYLIST, &["11".to_string(), "tidal:track:12".to_string()])
      .await
      .unwrap();

    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("GET /v1/playlists/3f2a-9c?"));
    let add = &requests[1];
    assert!(
      add.starts_with("POST /v1/playlists/3f2a-9c/items?"),
      "{add}"
    );
    assert!(add.contains("if-none-match: \"v7\""), "{add}");
    assert!(add.contains("trackIds=11%2C12"), "{add}");
    assert!(add.contains("toIndex=3"), "{add}");
    assert!(add.contains("onDupes=SKIP"), "{add}");
  }

  #[tokio::test]
  async fn removing_deletes_the_item_indices_under_the_fresh_etag() {
    let items = r#"{"items":[
      {"type":"track","item":{"id":1}},
      {"type":"video","item":{"id":5}},
      {"type":"track","item":{"id":2}}],"totalNumberOfItems":3}"#;
    let (base, server) = serve(vec![
      Reply::new("200 OK", items),
      state("\"v8\"", 3),
      Reply::new("200 OK", ""),
    ])
    .await;

    source_at(&base)
      .remove_tracks(PLAYLIST, &["1".to_string(), "2".to_string()])
      .await
      .unwrap();

    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("GET /v1/playlists/3f2a-9c/items?"));
    let delete = &requests[2];
    assert!(
      delete.starts_with("DELETE /v1/playlists/3f2a-9c/items/2,0?"),
      "{delete}"
    );
    assert!(delete.contains("if-none-match: \"v8\""), "{delete}");
  }

  #[tokio::test]
  async fn a_write_without_an_etag_is_refused_before_it_is_sent() {
    let (base, server) = serve(vec![Reply::new("200 OK", r#"{"uuid":"3f2a-9c"}"#)]).await;

    let err = source_at(&base)
      .add_tracks(PLAYLIST, &["11".to_string()])
      .await
      .unwrap_err();

    assert!(err.to_string().contains("ETag"), "{err:#}");
    assert_eq!(server.await.unwrap().len(), 1);
  }

  #[tokio::test]
  async fn a_playlist_is_created_in_the_root_folder_over_v2() {
    let (base, server) = serve(vec![Reply::new(
      "200 OK",
      r#"{"trn":"trn:playlist:ab-12","data":{"uuid":"ab-12","title":"Road Trip"}}"#,
    )])
    .await;

    let uuid = source_at(&base).create_playlist("Road Trip").await.unwrap();

    assert_eq!(uuid, "ab-12");
    let request = &server.await.unwrap()[0];
    assert!(
      request.starts_with("PUT /v2/my-collection/playlists/folders/create-playlist?"),
      "{request}"
    );
    assert!(request.contains("name=Road+Trip"), "{request}");
    assert!(request.contains("folderId=root"), "{request}");
  }

  #[tokio::test]
  async fn only_an_own_playlist_is_found_by_name() {
    let playlists = r#"{"items":[
      {"playlist":{"uuid":"theirs","title":"Road Trip","creator":{"id":7}}},
      {"playlist":{"uuid":"mine","title":"road trip ","creator":{"id":42}}}],
      "totalNumberOfItems":2}"#;
    let (base, _server) = serve(vec![Reply::new("200 OK", playlists)]).await;

    let found = source_at(&base)
      .own_playlist_named("Road Trip")
      .await
      .unwrap();

    assert_eq!(found.as_deref(), Some("tidal:playlist:mine"));
  }
}
