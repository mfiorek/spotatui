### Alternative sources (Local / Subsonic / Radio / YouTube / Qobuz / Tidal)

- Tidal (`src/infra/tidal/`) speaks the private client API
  (`api.tidal.com/v1`); the official developer API serves third-party clients
  30-second previews only. The OAuth client ID and secret are never embedded:
  they come from `config.yml` or the `SPOTATUI_TIDAL_*` env vars. Failures are
  status messages, never `handle_error`.
- Login (`auth.rs`, `client.rs`) is the OAuth device flow: `TidalLogin`
  first restores the saved login silently, and only runs the device flow
  (a `link.tidal.com` URL in the status bar) when that fails with
  `LoginRequired`. `tidal_credentials.yml` holds token state plus the client
  ID that minted it, never the secret; a different configured client ID asks
  for a new login. `TidalClient` refreshes while holding its token mutex, so
  concurrent callers share one refresh; every rotated token is saved.
- Browsing (`TidalSource` in `mod.rs`, serde types in `types.rs`) follows
  limit/offset pages by items received, 100 per page (50 for
  `playlistsAndFavoritePlaylists`, which refuses more), capped at 10k items.
  A browse uses the in-memory login, else restores the saved one inline, else
  dispatches `TidalLogin`, whose success reloads the sidebar. Ids go into
  request paths unescaped, so `listing_from_uri` admits only uuid characters.
- Playback (`dispatch.rs`, after Qobuz's) asks `playbackinfopostpaywall` for
  HIGH and decodes the BTS manifest (`manifest.rs`, pure): one direct,
  unencrypted CDN URL, AAC in MP4. A DASH (hi-res) manifest is an error for
  now. The track plays while it downloads: `stream.rs` runs a
  `stream-download` `HttpStream` (Range requests on seek) into the session's
  tempfile through the shared `infra/progressive.rs`. That client has a read
  timeout but no request timeout, which would cut tracks off; it is
  `stream-download`'s own `reqwest`, which can differ from the crate's. Errors
  never print the URL: its query carries the CDN token.
- The session lives in the **private** `App::tidal_playback` (accessors in
  `core/app/playback_routing.rs`; `pub_fields_on_app` may only fall), so the
  driver's `decoded_device_recovery!` / `decoded_auto_advance!` use their
  accessor arm. Repeat-one replays the tempfile only when `progressive`'s
  `Completion` says every byte arrived (`stream-download` back-fills a
  forward seek's gap at the end, unless the reader was dropped first);
  otherwise it fetches the track again.
- Not queueable yet: `add_track_to_native_queue` refuses `tidal:` URIs, and
  `App::tidal_ignores_native_queue` keeps the native queue from suspending a
  Tidal session (it has no `SuspendedContext` arm), so a Tidal list plays
  through and queued items wait until it ends.
