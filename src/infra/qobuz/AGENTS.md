### Alternative sources (Local / Subsonic / Radio / YouTube / Qobuz)

- Qobuz (`src/infra/qobuz/`) plays each track through the web player's
  encrypted CMAF stream while it downloads: `stream/progressive.rs` feeds the
  shared `infra/progressive.rs` `SegmentStream` (a `stream-download` source
  that restarts at a seek) with the decrypted segments, rebuilt as a
  `NamedTempFile` in the delivered format (FLAC or MP3) that the session keeps.
  The fetch runs off the pump behind a `fetch_id` guard; a superseded stream
  is dropped, which cancels its download. Request signing
  (`sign.rs`) is pure; `stream/` does the network and file I/O. Both are unit
  tested. The three web-player constants are scraped at runtime (`auth.rs`),
  cached in `state.yml`, and overridable through `SPOTATUI_QOBUZ_*` env vars;
  they are never embedded. Failures are status messages, never `handle_error`.
