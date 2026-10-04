use super::common_key_events;
use crate::core::action::{Action, OpenTarget};
use crate::core::app::{App, Artist, ArtistBlock};
use crate::core::plugin_api::TrackInfo;
use crate::tui::event::Key;

fn handle_down_press_on_selected_block(app: &mut App) {
  if let Some(artist) = &mut app.artist {
    match artist.artist_selected_block {
      ArtistBlock::TopTracks => {
        let next_index = common_key_events::on_down_press_handler(
          &artist.top_tracks,
          Some(artist.selected_top_track_index),
        );
        artist.selected_top_track_index = next_index;
      }
      ArtistBlock::Albums => {
        let next_index = common_key_events::on_down_press_handler(
          &artist.albums.items,
          Some(artist.selected_album_index),
        );
        artist.selected_album_index = next_index;
      }
      ArtistBlock::RelatedArtists => {
        let next_index = common_key_events::on_down_press_handler(
          &artist.related_artists,
          Some(artist.selected_related_artist_index),
        );
        artist.selected_related_artist_index = next_index;
      }
      ArtistBlock::Empty => {}
    }
  }
}

fn handle_down_press_on_hovered_block(app: &mut App) {
  if let Some(artist) = &mut app.artist {
    match artist.artist_hovered_block {
      ArtistBlock::TopTracks => {
        artist.artist_hovered_block = ArtistBlock::Albums;
      }
      ArtistBlock::Albums => {
        artist.artist_hovered_block = if artist.related_artists_visible() {
          ArtistBlock::RelatedArtists
        } else if artist.top_tracks_visible() {
          ArtistBlock::TopTracks
        } else {
          ArtistBlock::Albums
        };
      }
      ArtistBlock::RelatedArtists => {
        artist.artist_hovered_block = if artist.top_tracks_visible() {
          ArtistBlock::TopTracks
        } else {
          ArtistBlock::Albums
        };
      }
      ArtistBlock::Empty => {}
    }
  }
}

fn handle_up_press_on_selected_block(app: &mut App) {
  if let Some(artist) = &mut app.artist {
    match artist.artist_selected_block {
      ArtistBlock::TopTracks => {
        let next_index = common_key_events::on_up_press_handler(
          &artist.top_tracks,
          Some(artist.selected_top_track_index),
        );
        artist.selected_top_track_index = next_index;
      }
      ArtistBlock::Albums => {
        let next_index = common_key_events::on_up_press_handler(
          &artist.albums.items,
          Some(artist.selected_album_index),
        );
        artist.selected_album_index = next_index;
      }
      ArtistBlock::RelatedArtists => {
        let next_index = common_key_events::on_up_press_handler(
          &artist.related_artists,
          Some(artist.selected_related_artist_index),
        );
        artist.selected_related_artist_index = next_index;
      }
      ArtistBlock::Empty => {}
    }
  }
}

fn handle_up_press_on_hovered_block(app: &mut App) {
  if let Some(artist) = &mut app.artist {
    match artist.artist_hovered_block {
      ArtistBlock::TopTracks => {
        artist.artist_hovered_block = if artist.related_artists_visible() {
          ArtistBlock::RelatedArtists
        } else {
          ArtistBlock::Albums
        };
      }
      ArtistBlock::Albums => {
        artist.artist_hovered_block = if artist.top_tracks_visible() {
          ArtistBlock::TopTracks
        } else if artist.related_artists_visible() {
          ArtistBlock::RelatedArtists
        } else {
          ArtistBlock::Albums
        }
      }
      ArtistBlock::RelatedArtists => {
        artist.artist_hovered_block = ArtistBlock::Albums;
      }
      ArtistBlock::Empty => {}
    }
  }
}

fn handle_high_press_on_selected_block(app: &mut App) {
  if let Some(artist) = &mut app.artist {
    match artist.artist_selected_block {
      ArtistBlock::TopTracks => {
        let next_index = common_key_events::on_high_press_handler();
        artist.selected_top_track_index = next_index;
      }
      ArtistBlock::Albums => {
        let next_index = common_key_events::on_high_press_handler();
        artist.selected_album_index = next_index;
      }
      ArtistBlock::RelatedArtists => {
        let next_index = common_key_events::on_high_press_handler();
        artist.selected_related_artist_index = next_index;
      }
      ArtistBlock::Empty => {}
    }
  }
}

fn handle_middle_press_on_selected_block(app: &mut App) {
  if let Some(artist) = &mut app.artist {
    match artist.artist_selected_block {
      ArtistBlock::TopTracks => {
        let next_index = common_key_events::on_middle_press_handler(&artist.top_tracks);
        artist.selected_top_track_index = next_index;
      }
      ArtistBlock::Albums => {
        let next_index = common_key_events::on_middle_press_handler(&artist.albums.items);
        artist.selected_album_index = next_index;
      }
      ArtistBlock::RelatedArtists => {
        let next_index = common_key_events::on_middle_press_handler(&artist.related_artists);
        artist.selected_related_artist_index = next_index;
      }
      ArtistBlock::Empty => {}
    }
  }
}

fn handle_low_press_on_selected_block(app: &mut App) {
  if let Some(artist) = &mut app.artist {
    match artist.artist_selected_block {
      ArtistBlock::TopTracks => {
        let next_index = common_key_events::on_low_press_handler(&artist.top_tracks);
        artist.selected_top_track_index = next_index;
      }
      ArtistBlock::Albums => {
        let next_index = common_key_events::on_low_press_handler(&artist.albums.items);
        artist.selected_album_index = next_index;
      }
      ArtistBlock::RelatedArtists => {
        let next_index = common_key_events::on_low_press_handler(&artist.related_artists);
        artist.selected_related_artist_index = next_index;
      }
      ArtistBlock::Empty => {}
    }
  }
}

/// The top track under the cursor.
fn selected_top_track(artist: &Artist) -> Option<TrackInfo> {
  artist
    .top_tracks
    .get(artist.selected_top_track_index)
    .cloned()
}

fn top_track_playback_request(artist: &Artist) -> (Vec<String>, Option<usize>) {
  common_key_events::uri_playback_request(
    artist.top_tracks.iter().map(|track| track.uri.clone()),
    artist.selected_top_track_index,
  )
}

/// The album id under the artist page's album cursor.
fn selected_album_id(artist: &Artist) -> Option<String> {
  artist
    .albums
    .items
    .get(artist.selected_album_index)?
    .id
    .clone()
}

/// `None` when the row is missing or has no id: the related-artists list is
/// often empty while the cursor still reports 0.
fn selected_related_artist(artist: &Artist) -> Option<(String, String)> {
  let related = artist
    .related_artists
    .get(artist.selected_related_artist_index)?;
  Some((related.id.clone()?, related.name.clone()))
}

/// The decoded-source URI of the row under the selected block's cursor;
/// `None` on a Spotify artist page.
fn selected_source_row_uri(artist: &Artist) -> Option<String> {
  let uri = match artist.artist_selected_block {
    ArtistBlock::TopTracks => artist
      .top_tracks
      .get(artist.selected_top_track_index)?
      .uri
      .as_deref(),
    ArtistBlock::Albums => artist
      .albums
      .items
      .get(artist.selected_album_index)?
      .uri
      .as_deref(),
    ArtistBlock::RelatedArtists => artist
      .related_artists
      .get(artist.selected_related_artist_index)?
      .uri
      .as_deref(),
    ArtistBlock::Empty => None,
  };
  common_key_events::source_row_uri(uri)
}

fn handle_recommend_event_on_selected_block(app: &mut App) {
  let Some(artist) = &app.artist else {
    return;
  };
  match artist.artist_selected_block {
    ArtistBlock::TopTracks => {
      // The full track: the results table prepends the seed as a context row.
      if let Some(track) = selected_top_track(artist) {
        app.apply(Action::RecommendFromTrack(track));
      }
    }
    ArtistBlock::RelatedArtists => {
      // ArtistInfo.id is Option<String>; only dispatch if an id is present to
      // avoid a seed-less recommendation call that returns garbage results.
      if let Some((id, name)) = selected_related_artist(artist) {
        app.apply(Action::RecommendFromArtist { id, name });
      }
    }
    _ => {}
  }
}

fn handle_enter_event_on_selected_block(app: &mut App) {
  let Some(artist) = &app.artist else {
    return;
  };
  match artist.artist_selected_block {
    ArtistBlock::TopTracks => {
      let (uris, offset) = top_track_playback_request(artist);
      app.apply(Action::PlayUris { uris, offset });
    }
    ArtistBlock::Albums => {
      if let Some(uri) = selected_source_row_uri(artist) {
        app.apply(Action::Open(OpenTarget::SourcePlaylist(uri)));
        return;
      }
      // GetAlbum fetches a FullAlbum and sets AlbumTableContext::Full — do NOT
      // set track_table.context here, as GetAlbum does not use the track table.
      if let Some(id) = selected_album_id(artist) {
        app.apply(Action::Open(OpenTarget::Album {
          id,
          from_search: false,
        }));
      }
    }
    ArtistBlock::RelatedArtists => {
      if let Some(uri) = selected_source_row_uri(artist) {
        let name = artist
          .related_artists
          .get(artist.selected_related_artist_index)
          .map(|related| related.name.clone())
          .unwrap_or_default();
        app.apply(Action::Open(OpenTarget::SourceArtist { uri, name }));
        return;
      }
      if let Some((id, name)) = selected_related_artist(artist) {
        app.apply(Action::Open(OpenTarget::Artist { id, name }));
      }
    }
    ArtistBlock::Empty => {}
  }
}

fn handle_enter_event_on_hovered_block(app: &mut App) {
  if let Some(artist) = &mut app.artist {
    match artist.artist_hovered_block {
      ArtistBlock::TopTracks if artist.top_tracks_visible() => {
        artist.artist_selected_block = ArtistBlock::TopTracks;
      }
      ArtistBlock::Albums => artist.artist_selected_block = ArtistBlock::Albums,
      ArtistBlock::RelatedArtists => artist.artist_selected_block = ArtistBlock::RelatedArtists,
      ArtistBlock::Empty => {}
      ArtistBlock::TopTracks => {}
    }
  }
}

pub fn handler(key: Key, app: &mut App) {
  let source_uri = app.artist.as_ref().and_then(selected_source_row_uri);
  if let Some(artist) = &mut app.artist {
    match key {
      Key::Esc => {
        artist.artist_selected_block = ArtistBlock::Empty;
      }
      k if common_key_events::down_event(k, &app.user_config.keys) => {
        if artist.artist_selected_block != ArtistBlock::Empty {
          handle_down_press_on_selected_block(app);
        } else {
          handle_down_press_on_hovered_block(app);
        }
      }
      k if common_key_events::up_event(k, &app.user_config.keys) => {
        if artist.artist_selected_block != ArtistBlock::Empty {
          handle_up_press_on_selected_block(app);
        } else {
          handle_up_press_on_hovered_block(app);
        }
      }
      k if common_key_events::left_event(k, &app.user_config.keys) => {
        artist.artist_selected_block = ArtistBlock::Empty;
        match artist.artist_hovered_block {
          ArtistBlock::TopTracks => common_key_events::handle_left_event(app),
          ArtistBlock::Albums => {
            if artist.top_tracks_visible() {
              artist.artist_hovered_block = ArtistBlock::TopTracks;
            } else {
              common_key_events::handle_left_event(app);
            }
          }
          ArtistBlock::RelatedArtists => {
            artist.artist_hovered_block = ArtistBlock::Albums;
          }
          ArtistBlock::Empty => {}
        }
      }
      k if common_key_events::right_event(k, &app.user_config.keys) => {
        artist.artist_selected_block = ArtistBlock::Empty;
        handle_down_press_on_hovered_block(app);
      }
      k if common_key_events::high_event(k)
        && artist.artist_selected_block != ArtistBlock::Empty =>
      {
        handle_high_press_on_selected_block(app);
      }
      k if common_key_events::middle_event(k)
        && artist.artist_selected_block != ArtistBlock::Empty =>
      {
        handle_middle_press_on_selected_block(app);
      }
      k if common_key_events::low_event(k)
        && artist.artist_selected_block != ArtistBlock::Empty =>
      {
        handle_low_press_on_selected_block(app);
      }
      Key::Enter => {
        if artist.artist_selected_block != ArtistBlock::Empty {
          handle_enter_event_on_selected_block(app);
        } else {
          handle_enter_event_on_hovered_block(app);
        }
      }
      // A decoded source's page: recommending, saving and following are
      // Spotify calls. Its top tracks keep `w` like the search songs block.
      Key::Char('r') | Key::Char('w') | Key::Char('D')
        if source_uri.is_some()
          && (key == Key::Char('r') || artist.artist_selected_block != ArtistBlock::TopTracks) =>
      {
        if let Some(uri) = &source_uri {
          common_key_events::refuse_spotify_only_key(app, uri);
        }
      }
      Key::Char('r') if artist.artist_selected_block != ArtistBlock::Empty => {
        handle_recommend_event_on_selected_block(app);
      }
      Key::Char('w') => match artist.artist_selected_block {
        ArtistBlock::TopTracks => {
          let identity = app
            .artist
            .as_ref()
            .and_then(|artist| artist.top_tracks.get(artist.selected_top_track_index))
            .map(|track| (track.id.clone(), track.name.clone()));
          if let Some((track_id, track_name)) = identity {
            app.apply(Action::OpenAddTrackDialogFor {
              track_id,
              track_name,
            });
          }
        }
        ArtistBlock::Albums => {
          if let Some(id) = artist_page_album_id(app) {
            app.apply(Action::SaveAlbum(id));
          }
        }
        ArtistBlock::RelatedArtists => {
          if let Some((id, _)) = artist_page_related_artist(app) {
            app.apply(Action::FollowArtist(id));
          }
        }
        _ => (),
      },
      Key::Char('D') => match artist.artist_selected_block {
        ArtistBlock::Albums => {
          if let Some(id) = artist_page_album_id(app) {
            app.apply(Action::UnsaveAlbum(id));
          }
        }
        ArtistBlock::RelatedArtists => {
          if let Some((id, _)) = artist_page_related_artist(app) {
            app.apply(Action::UnfollowArtist(id));
          }
        }
        _ => (),
      },
      _ if key == app.user_config.keys.add_item_to_queue => {
        let track = app
          .artist
          .as_ref()
          .filter(|artist| artist.artist_selected_block == ArtistBlock::TopTracks)
          .and_then(selected_top_track);
        if let Some(track) = track {
          app.apply(Action::QueueTrack(track));
        }
      }
      _ => {}
    };
  }
}

/// Re-read from the whole `App`: the key match's borrow of `app.artist` has
/// ended by the time an arm body runs.
fn artist_page_album_id(app: &App) -> Option<String> {
  app.artist.as_ref().and_then(selected_album_id)
}

fn artist_page_related_artist(app: &App) -> Option<(String, String)> {
  app.artist.as_ref().and_then(selected_related_artist)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::core::app::ActiveBlock;
  use crate::core::pagination::Paged;
  use crate::core::plugin_api::{ArtistInfo, TrackInfo};

  fn track(id: &str, name: &str) -> TrackInfo {
    TrackInfo {
      id: Some(id.to_string()),
      name: name.to_string(),
      uri: Some(format!("spotify:track:{id}")),
      artists: vec!["First".to_string()],
      album: "Album".to_string(),
      duration_ms: 1_000,
      album_id: None,
      artist_refs: vec![],
      is_playable: true,
      is_local: false,
      track_number: 0,
      explicit: false,
      image_url: None,
    }
  }

  fn artist_page_with_tracks(
    top_tracks: Vec<TrackInfo>,
    related_artists: Vec<ArtistInfo>,
  ) -> Artist {
    Artist {
      artist_id: "artist1".to_string(),
      artist_name: "First".to_string(),
      albums: Paged::default(),
      related_artists,
      top_tracks,
      selected_album_index: 0,
      selected_related_artist_index: 0,
      selected_top_track_index: 0,
      artist_hovered_block: ArtistBlock::TopTracks,
      artist_selected_block: ArtistBlock::RelatedArtists,
    }
  }

  fn artist_page(related_artists: Vec<ArtistInfo>) -> Artist {
    // Default page has 1 track so top_tracks_visible() is true for standard 3-block tests
    artist_page_with_tracks(vec![track("t1", "Track 1")], related_artists)
  }

  fn empty_artist_page(related_artists: Vec<ArtistInfo>) -> Artist {
    // Empty tracks for testing dev-mode / visibility-skipping behavior
    artist_page_with_tracks(vec![], related_artists)
  }

  fn related_artist(id: &str, name: &str) -> ArtistInfo {
    ArtistInfo {
      id: Some(id.to_string()),
      uri: Some(format!("spotify:artist:{id}")),
      name: name.to_string(),
      image_url: None,
    }
  }

  fn hovered_block(app: &App) -> ArtistBlock {
    app
      .artist
      .as_ref()
      .expect("artist page should be set")
      .artist_hovered_block
      .clone()
  }

  fn app_hovering(page: Artist) -> App {
    app_hovering_with(App::default(), page)
  }

  fn app_hovering_with(mut app: App, page: Artist) -> App {
    app.artist = Some(page);
    app
  }

  #[test]
  fn down_cycle_keeps_the_three_block_order_when_related_artists_exist() {
    let mut app = app_hovering(artist_page(vec![related_artist("artist2", "Second")]));

    handle_down_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::Albums);
    handle_down_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::RelatedArtists);
    handle_down_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::TopTracks);
  }

  #[test]
  fn up_cycle_keeps_the_three_block_order_when_related_artists_exist() {
    let mut app = app_hovering(artist_page(vec![related_artist("artist2", "Second")]));

    handle_up_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::RelatedArtists);
    handle_up_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::Albums);
    handle_up_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::TopTracks);
  }

  #[test]
  fn cycles_skip_the_related_artists_block_when_the_list_is_empty() {
    let mut app = app_hovering(artist_page(vec![]));

    // Down: TopTracks → Albums, then wraps back to TopTracks (skips RelatedArtists)
    handle_down_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::Albums);
    handle_down_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::TopTracks);

    // Up from TopTracks lands on Albums
    handle_up_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::Albums);
    handle_up_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::TopTracks);
  }

  #[test]
  fn cycles_skip_top_tracks_when_not_visible() {
    let mut page = empty_artist_page(vec![related_artist("artist2", "Second")]);
    page.artist_hovered_block = ArtistBlock::Albums;
    let mut app = app_hovering(page);

    // Down: Albums → RelatedArtists, then wraps straight to Albums (skipping TopTracks)
    handle_down_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::RelatedArtists);
    handle_down_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::Albums);

    // Up: Albums → RelatedArtists, wraps back to Albums (skipping TopTracks)
    handle_up_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::RelatedArtists);
    handle_up_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::Albums);
  }

  #[test]
  fn cycles_stay_on_albums_when_both_top_tracks_and_related_artists_are_empty() {
    let mut page = empty_artist_page(vec![]);
    page.artist_hovered_block = ArtistBlock::Albums;
    let mut app = app_hovering(page);

    // Only Albums exists: pressing down or up stays on Albums
    handle_down_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::Albums);

    handle_up_press_on_hovered_block(&mut app);
    assert_eq!(hovered_block(&app), ArtistBlock::Albums);
  }

  #[test]
  fn left_press_from_albums_moves_to_top_tracks_when_visible() {
    let mut page = artist_page(vec![]);
    page.artist_hovered_block = ArtistBlock::Albums;
    let mut app = app_hovering(page);

    handler(Key::Left, &mut app);

    assert_eq!(hovered_block(&app), ArtistBlock::TopTracks);
  }

  #[test]
  fn left_press_from_albums_exits_artist_view_when_top_tracks_not_visible() {
    let mut page = empty_artist_page(vec![]);
    page.artist_hovered_block = ArtistBlock::Albums;
    let mut app = app_hovering(page);

    handler(Key::Left, &mut app);

    // Does NOT get stuck on TopTracks; exits or resets selected block
    assert_ne!(hovered_block(&app), ArtistBlock::TopTracks);
    assert_eq!(
      app.artist.as_ref().unwrap().artist_selected_block,
      ArtistBlock::Empty
    );
  }

  #[test]
  fn enter_on_hovered_top_tracks_ignored_when_not_visible() {
    let mut page = empty_artist_page(vec![]);
    page.artist_hovered_block = ArtistBlock::TopTracks;
    page.artist_selected_block = ArtistBlock::Empty;
    let mut app = app_hovering(page);

    handle_enter_event_on_hovered_block(&mut app);

    // Remains unselected because top tracks are hidden/empty!
    assert_eq!(
      app.artist.as_ref().unwrap().artist_selected_block,
      ArtistBlock::Empty
    );
  }

  #[test]
  fn enter_on_hovered_top_tracks_selects_when_visible() {
    let mut page = artist_page(vec![]);
    page.artist_hovered_block = ArtistBlock::TopTracks;
    page.artist_selected_block = ArtistBlock::Empty;
    let mut app = app_hovering(page);

    handle_enter_event_on_hovered_block(&mut app);

    assert_eq!(
      app.artist.as_ref().unwrap().artist_selected_block,
      ArtistBlock::TopTracks
    );
  }

  #[test]
  fn on_esc() {
    let mut app = App::default();

    handler(Key::Esc, &mut app);

    let current_route = app.get_current_route();
    assert_eq!(current_route.active_block, ActiveBlock::Empty);
  }

  #[test]
  fn r_on_a_top_track_seeds_recommendations_with_the_track_as_the_context_row() {
    use crate::core::user_config::UserConfig;
    use crate::infra::network::IoEvent;
    use std::sync::mpsc::channel;
    use std::time::SystemTime;

    let (tx, rx) = channel();
    let mut page = artist_page_with_tracks(vec![track("one", "One")], vec![]);
    page.artist_selected_block = ArtistBlock::TopTracks;
    let mut app = app_hovering_with(
      App::new(tx, UserConfig::new(), Some(SystemTime::now())),
      page,
    );

    handler(Key::Char('r'), &mut app);

    match rx.try_recv() {
      Ok(IoEvent::GetRecommendationsForSeed(None, seed_tracks, first_track, _)) => {
        assert_eq!(seed_tracks, Some(vec!["spotify:track:one".to_string()]));
        assert_eq!(
          (*first_track).map(|track| track.name),
          Some("One".to_string())
        );
      }
      _ => panic!("expected GetRecommendationsForSeed with the top track as first_track"),
    }
    assert_eq!(app.recommendations_seed, "One");
  }

  #[test]
  fn related_artist_lookup_on_an_empty_list_resolves_nothing() {
    assert_eq!(selected_related_artist(&empty_artist_page(vec![])), None);
  }

  #[test]
  fn related_artist_lookup_returns_the_row_identity() {
    let page = empty_artist_page(vec![ArtistInfo {
      id: Some("artist2".to_string()),
      uri: Some("spotify:artist:artist2".to_string()),
      name: "Second".to_string(),
      image_url: None,
    }]);

    assert_eq!(
      selected_related_artist(&page),
      Some(("artist2".to_string(), "Second".to_string()))
    );
  }

  #[test]
  fn related_artist_lookup_without_an_id_resolves_nothing() {
    let page = empty_artist_page(vec![ArtistInfo {
      id: None,
      uri: None,
      name: "Unknown".to_string(),
      image_url: None,
    }]);

    assert_eq!(selected_related_artist(&page), None);
  }

  /// A Tidal artist page focused on `block`: one top track, album and
  /// similar artist, all carrying `tidal:` URIs.
  fn tidal_app_on(
    block: ArtistBlock,
  ) -> (
    App,
    std::sync::mpsc::Receiver<crate::infra::network::IoEvent>,
  ) {
    let (tx, rx) = std::sync::mpsc::channel();
    let app = App::new(
      tx,
      crate::core::user_config::UserConfig::new(),
      Some(std::time::SystemTime::now()),
    );
    let mut top = track("11", "Self Esteem");
    top.uri = Some("tidal:track:11".to_string());
    let mut page = artist_page_with_tracks(
      vec![top],
      vec![ArtistInfo {
        id: Some("8".to_string()),
        uri: Some("tidal:artist:8".to_string()),
        name: "Bad Religion".to_string(),
        image_url: None,
      }],
    );
    page.albums = Paged {
      items: vec![crate::core::plugin_api::AlbumInfo {
        id: Some("5".to_string()),
        uri: Some("tidal:album:5".to_string()),
        name: "Smash".to_string(),
        ..Default::default()
      }],
      total: 1,
      ..Default::default()
    };
    page.artist_selected_block = block;
    (app_hovering_with(app, page), rx)
  }

  #[test]
  fn enter_on_a_tidal_album_opens_it_through_tidal() {
    let (mut app, rx) = tidal_app_on(ArtistBlock::Albums);

    handler(Key::Enter, &mut app);

    assert!(rx.try_recv().is_ok(), "the album is fetched");
    assert_eq!(
      app.track_table.context,
      Some(crate::core::app::TrackTableContext::TidalPlaylist)
    );
  }

  /// Enter opens `OpenTarget::SourceArtist` with this URI; the action tests
  /// pin where that goes.
  #[test]
  fn a_similar_tidal_artist_is_routed_by_its_uri() {
    let (app, _rx) = tidal_app_on(ArtistBlock::RelatedArtists);

    assert_eq!(
      selected_source_row_uri(app.artist.as_ref().unwrap()).as_deref(),
      Some("tidal:artist:8")
    );
  }

  #[test]
  fn a_spotify_artist_page_is_not_routed_to_a_source() {
    let page = artist_page(vec![related_artist("artist2", "Second")]);

    assert_eq!(selected_source_row_uri(&page), None);
  }

  #[test]
  fn spotify_library_keys_on_a_tidal_page_send_nothing_to_spotify() {
    for (block, keys) in [
      (ArtistBlock::Albums, &['w', 'D', 'r'][..]),
      (ArtistBlock::RelatedArtists, &['w', 'D', 'r'][..]),
      (ArtistBlock::TopTracks, &['r'][..]),
    ] {
      let (mut app, rx) = tidal_app_on(block);
      for key in keys {
        handler(Key::Char(*key), &mut app);
        assert!(rx.try_recv().is_err(), "{key}");
        assert_eq!(
          app.status_message(),
          Some("Not available for Tidal results")
        );
      }
    }
  }
}
