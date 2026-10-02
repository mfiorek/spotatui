### Alternative sources (Local / Subsonic / Radio / YouTube / Qobuz / Tidal)

- Tidal (`src/infra/tidal/`) speaks the private client API
  (`api.tidal.com/v1`); the official developer API serves third-party clients
  30-second previews only. The OAuth client ID and secret are never embedded:
  they come from `config.yml` or the `SPOTATUI_TIDAL_*` env vars. Failures are
  status messages, never `handle_error`.
