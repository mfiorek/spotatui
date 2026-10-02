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
  the `behavior.tidal_quality` tier, read when each fetch starts:
  HI_RES_LOSSLESS (the default), HIGH or LOW (`manifest.rs`, pure). LOSSLESS
  is not offered: this client type gets it as HIGH. A hi-res request for a
  track with a hi-res master comes back as unencrypted MPEG-DASH FLAC; any
  other comes back over BTS: one direct, unencrypted CDN URL, AAC in a plain
  (not fragmented) MP4.
  Either way the track plays while it downloads into the session's tempfile
  through the shared `infra/progressive.rs`.
  - BTS: `stream.rs` runs a `stream-download` `HttpStream` (Range requests on
    seek). That client has a read timeout but no request timeout, which would
    cut tracks off; it is `stream-download`'s own `reqwest`, which can differ
    from the crate's.
  - DASH: the fMP4 is **not** passed through: symphonia's MP4 demuxer walks
    every top-level box of a seekable source, a range request per segment.
    `dash.rs` (pure) parses the MPD and the init segment's `dfLa` box;
    `segments.rs` measures each segment's `mdat` with a 1 KiB Range request
    (8 at a time) before playback, then feeds the shared `SegmentStream` the
    FLAC header and each segment's `mdat` payload: a raw FLAC file the FLAC
    demuxer seeks in. A payload whose size differs from the measured one is
    an error, since every later offset depends on it.
  - A DASH stream that fails to open is asked for again as HIGH (BTS); only
    a hi-res request falls back (`fallback_quality`). The
    playbar shows what was delivered: `FLAC 24/48` read from STREAMINFO, or
    `AAC 320`.
  - Errors never print a URL: its query carries the CDN token.
  - `probe.rs` is an ignored live test that prints the MPD and box layouts
    (`live_tidal_probe`); run it when the delivery seems to have changed.
- The session lives in the **private** `App::tidal_playback` (accessors in
  `core/app/playback_routing.rs`; `pub_fields_on_app` may only fall), so the
  driver's `decoded_device_recovery!` / `decoded_auto_advance!` use their
  accessor arm. Repeat-one replays the tempfile only when `progressive`'s
  `Completion` says every byte arrived (`stream-download` back-fills a
  forward seek's gap at the end, unless the reader was dropped first);
  otherwise it fetches the track again.
- The native queue treats Tidal as Qobuz: `SuspendedContext::Tidal` aborts the
  fetch in flight and lends the session's player to the queue slot, and a
  fetch that lands while the queue owns the sink leaves the session for the
  resume. A queued Tidal track is downloaded whole first
  (`dispatch::download_for_queue`). The resume replays the tempfile only when
  `file_is_complete()`: the slot dropped the progressive reader, which stops
  its download. Only `tidal:track:` URIs are playable (`is_playable_track_uri`);
  playlist and album URIs share the scheme.
- Playlist sync (`playlist_sync.rs`) uses the private-API writes python-tidal
  sends. Every add or remove reads the playlist's `ETag` first and sends it
  as `If-None-Match`; a missing `ETag` refuses the write. Creation is the v2
  `my-collection/playlists/folders/create-playlist` (into `root`), the only
  v2 call. Removal deletes by item index, last copy first and highest index
  first, from `playlists/<uuid>/items`, whose indices count videos too. A
  mirror is adopted by name only among the user's own playlists (creator id
  equals user id); a followed one can be a master. A sync run never starts
  the device login (`dispatch::build_sync_source`), since it can run from the
  CLI. `live_tidal_playlist_sync` (ignored; it writes to the account) runs
  every write against a temporary playlist it deletes afterwards.
