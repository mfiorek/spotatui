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
