# Tidal source: scope

Working notes for adding Tidal as a source on branch `feat/tidal-source`. The
starting point is upstream `fd9fb67` (2026-10-01). This is a planning document
for the fork. It is not user-facing docs.

## Goal

Add Tidal as a source behind a `tidal` cargo feature, working in the fork. Use
it day to day. Only once it works well, decide whether to propose it to
[LargeModGames/spotatui](https://github.com/LargeModGames/spotatui). That
decision is deliberately deferred: nothing upstream happens until then.

## Decisions

- **Extend spotatui, not another client.** spotatui already plays several
  sources through its own decode pipeline, so the visualizer, volume curve and
  queue work across all of them.
- **Use the private client API.** That is `api.tidal.com/v1`, the API the
  python-tidal ecosystem uses. The official developer API only serves
  30-second previews to third-party clients.
- **Port from cliamp.** [bjarneo/cliamp](https://github.com/bjarneo/cliamp),
  `external/tidal/` (Go, MIT, ~2.7k lines incl. tests; docs in
  `docs/tidal.md`). Its files: `auth.go`, `creds.go`, `client.go`,
  `types.go`, `manifest.go`, `dash.go`, `provider.go`, `probe.go`.
- **Fork first, upstream later (maybe).** Build and use it in the fork with
  no upstream issue. If it proves worth merging, open the issue then. It is
  CONTRIBUTING.md's required first step for a new feature, so it comes before
  any PR.
- **Build to upstream standards anyway.** Follow the repo rules below and keep
  the CI matrix green, so an upstream PR stays a cheap option rather than a
  rewrite.
- **Git workflow.** Keep the fork's `main` identical to `upstream/main` and
  rebase this branch on it often. Upstream is very active, and this change
  touches many files; the longer the branch lives, the more this matters.

## Decisions for the fork (revisit before upstreaming)

1. **Borrowed client ID.** cliamp ships the client ID and secret that
   open-source Tidal clients share. Tidal revokes these periodically.
   - Fork: read them from config or `SPOTATUI_TIDAL_*` env vars and keep them
     out of the source tree, so nothing needs undoing later.
   - Later: whether upstream accepts any default ID is the maintainer's call
     and the first thing to ask in the issue. The alternative is scraping
     the constants at runtime, as Qobuz does with nothing secret embedded.
2. **AAC fallback.** With this client type, Tidal serves FLAC only for tracks
   that have a hi-res master. Every other track is downgraded server-side to
   AAC 320. An Android-type PKCE client would unlock FLAC for the full
   catalog; cliamp lists that as planned.
   - Fork: accept AAC for a first version, and show the delivered format in
     the playbar so the fallback is visible.
   - Later: try the PKCE client once playback works.
3. **Commit slicing.** Keep login/browsing/search and streaming in separate,
   self-contained commits. Whether to send one PR or two can then be decided
   at upstream time without re-cutting history.

## Tidal API notes (from cliamp)

- **Auth:** OAuth 2.0 device flow. The user opens `link.tidal.com` and enters
  a short code. The app polls for up to 5 minutes, then refreshes the token
  silently.
- **Playback:** the per-track `playbackinfopostpaywall` endpoint returns a
  manifest:
  - LOW and HIGH (AAC 96 / 320) come back as **BTS** manifests: direct CDN
    URLs, unencrypted.
  - This client type never gets LOSSLESS over BTS: a LOSSLESS request is
    capped at HIGH AAC (cliamp verified this against the live API).
  - HI_RES_LOSSLESS comes back as an **unencrypted MPEG-DASH** FLAC manifest
    when the track has a hi-res master, and is downgraded to HIGH AAC over
    BTS otherwise. The reply's `audioQuality` names what was delivered.
  - Nothing needs decrypting, so Qobuz's `stream/crypto.rs` and most of
    `stream/cmaf.rs` do not carry over.
- **Library:** favorite tracks (cliamp caps them at 500), favorite artists →
  albums → tracks, favorite albums, user playlists, and catalog search (tracks
  and albums).

## How a source fits spotatui

- `src/core/source.rs`:
  - The `Source` enum: add `Tidal` to the enum, `ALL`, `label()` and
    `note()`.
  - The capability flags `supports_search`, `supports_library`,
    `supports_playlist_write`, `supports_like` and `supports_playlist_sync`.
    Tests assert these.
  - The traits `MediaSource`, `Searcher`, `LibraryProvider`, `PlaylistWriter`
    and `Streamer`. They speak `core::plugin_api` domain types, and dispatch
    goes through a closed enum, never `dyn`.
- **Routing:** `src/runtime/pump.rs` intercepts by URI scheme. Add a
  `route_tidal_event` next to `route_qobuz_event`, and call
  `claim_decoded_sink(Source::Tidal)` before playback starts.
- **Playback:** decoded through the shared `LocalPlayer`
  (`src/infra/audio/player.rs`). Rodio types appear only in that file.
- **Proposed URIs:** `tidal:track:<id>`, `tidal:favorites:tracks`,
  `tidal:playlist:<id>`, `tidal:album:<id>`.

## Template: the Qobuz source

- **Module:** `src/infra/qobuz/`, which holds `auth.rs`, `sign.rs`,
  `types.rs`, `mod.rs`, `dispatch.rs`, `stream/{cmaf,crypto,download,progressive}.rs`
  and `AGENTS.md`.
- **Feature:** `qobuz = ["audio-decode-queue", "queue", "queue-download",
  "reqwest/stream", "dep:stream-download", ...]`. Tidal needs roughly the
  same set minus the crypto crates, plus an XML parser for DASH.
- **PR:** #489 (commit `9ba6868`). Read its description and file list before
  starting. Points from it:
  - Besides `Cargo.toml`, the feature was wired into `.github/workflows/cd.yml`
    (the Linux and Windows release rows) and `ci.yml`. macOS release builds
    ship no decoded sources.
  - Its live test, `cargo test --features qobuz -- --ignored live_qobuz --nocapture`,
    is the pattern to copy for Tidal.
  - Known follow-ups it listed: repeat-one after a forward seek can replay a
    tempfile with an unfilled gap. Check this before reusing
    `stream/progressive.rs`.
- **`QobuzPlaybackState`** (`src/infra/qobuz/mod.rs`) is the model for
  `TidalPlaybackState`: `player`, `source`, `tracks`, `index`, `advancing`,
  `tempfile`, `quality`, `shuffle_backup`, `fetch_id`, `resume_at`, `fetch`.

### Finding every touch point

```bash
grep -rln -i qobuz src docs gui/src *.md Cargo.toml .github | grep -v src/infra/qobuz
```

The compiler catches exhaustive `match`es only. Many touch points fail
silently, so walk the grep list, not just the build errors:

- `#[cfg(feature = "qobuz")]` blocks;
- `cfg(any(feature = ...))` lists of the decoded sources;
- `if source == Source::Qobuz` comparisons.

Largest touch points, by number of Qobuz mentions at `fd9fb67`:
`infra/playlist_sync/run.rs` (61), `infra/queue/dispatch.rs` (31),
`infra/playlist_sync/mod.rs` (25), `infra/network/mod.rs` (25),
`tui/ui/player.rs` (21), `core/first_run.rs` (20), `core/action/tests.rs`
(19), `core/user_config.rs` (18), `core/state.rs` (18),
`core/app/playback_routing.rs` (17), `core/queue.rs` (15),
`core/source.rs` (14), `runtime/pump.rs` (12).

Outside `src/`:

- `gui/src/` (libraryModel, healthModel, format, plus tests).
- The `README.md`, `CHANGELOG.md` and `AGENTS.md` files, and
  `docs/{configuration,installation,playlist-sync,scripting}.md`.

## Repo rules that apply

From `AGENTS.md` and the per-directory `AGENTS.md` files:

- **Errors:** report auth and stream failures as status messages, never
  through `App::handle_error`. `handle_error` also drives CLI exit codes.
- **Credentials:**
  - Store the token in a new `tidal_credentials.yml` in the config dir,
    written with `core::auth::write_private_file_atomic`, with its path
    resolved in `core/paths.rs`.
  - Add a row for the file to the on-disk files table in `AGENTS.md`.
  - A refreshed token is machine-written, so it never goes in `config.yml`.
    A user's client-ID override may.
- **Quality setting:** `behavior.tidal_quality` needs edits in five places in
  `core/user_config.rs`: `BehaviorConfigString`, `BehaviorConfig`,
  `UserConfig::new`, `load_behaviorconfig` and `save_config`. It also needs
  arms in `core/app/settings_schema.rs` and `settings_apply.rs`. The parts are
  linked only by the raw string id, so a typo silently drops the setting.
- **Track changes:**
  - Set `advancing` synchronously before dispatching a track change.
  - Guard each download with `fetch_id` and abort the superseded fetch.
  - Publish the per-source `Option` on `App` only on success.
- **Playback ownership:** follow the predicate order in
  `src/core/app/AGENTS.md`. `media_metadata.rs` checks decoded sources first.
- **Gates ratchet:** coupling counters in `tools/gates.count` may only fall.
  Add no new `pub` fields on `App` and no writes to `view` outside `tui/` and
  `core/app/`.
- **GUI bindings:** `Source` derives `ts_rs::TS`. A `gui` test run
  regenerates `gui/src/bindings/Source.ts`; commit it, or the `gui` CI leg
  fails.
- **Docs:** add `src/infra/tidal/AGENTS.md` (plus the `CLAUDE.md` import
  stub) and a row for it in the root "Domain-specific conventions" table.

## Plan (fork)

1. **Read first.** Root `AGENTS.md`; the `AGENTS.md` files in
   `src/infra/{qobuz,audio,queue,network}/` and `src/core/app/`; all of
   `src/infra/qobuz/`; and PR #489.
2. **Skeleton.**
   - Add the `tidal` feature and include it in `all-sources`.
   - Add `Source::Tidal` with its capability flags and tests.
   - Add an empty `src/infra/tidal/` and regenerate the GUI bindings.
3. **Auth.**
   - Port the device flow, credential storage and silent refresh.
   - Read the client ID and secret from config or env, never from the source
     tree.
   - Add the first-run and onboarding steps (TUI and `gui/onboarding.rs`).
4. **Browsing.**
   - Port the API client and types, mapped into `core::plugin_api`.
   - Add sidebar rows (favorite tracks, playlists, favorite albums) and
     search.
   - Add `TrackTableContext::TidalPlaylist` and the `core/requirement.rs`
     rows.
5. **Streaming: BTS.**
   - Fetch direct URLs into a `NamedTempFile` through the Qobuz
     download/queue path.
   - Add `TidalPlaybackState`, routing and the playback-ownership arms.
   - Add the playbar format label, with the AAC fallback visible.

   **Milestone: usable in the TUI.** Build with `--features tidal` and use
   it daily; let real use drive what comes next.
6. **Streaming: DASH hi-res.** Parse the manifest, then fetch and concatenate
   the segments, seeking via `stream-download` as Qobuz does.
7. **Integrations.**
   - The native queue, `queue_suspend`, session restore
     (`persisted_playback.rs`) and shuffle/repeat.
   - MPRIS/SMTC/Discord metadata, cover art and the play counter.
   - Playlist sync and the DJ tools.
   - The `gui/` frontend.
8. **Setting.** `behavior.tidal_quality`. Optionally try the Android PKCE
   client for full-catalog FLAC.
9. **Tests, alongside each step.**
   - Unit tests for the pure parts: manifest parsing, DASH, mapping, URI
     parsing.
   - An `#[ignore]`d `live_tidal` test.
   - Colocated `#[cfg(test)]` modules, with test names that read as behavior
     sentences.

## Checks while working in the fork

Run the fast gate before each commit, and the full source build at each step:

```bash
cargo fmt --all
cargo clippy --no-default-features --features telemetry,tui -- -D warnings
cargo test --no-default-features --features telemetry,tui
cargo clippy --features all-sources -- -D warnings
cargo test --features all-sources
cargo test --features tidal -- --ignored live_tidal --nocapture --test-threads=1
```

The live tests run one at a time: the API client (`shared_tidal_client`) is
one per process, and each `#[tokio::test]` has its own runtime, so a pooled
connection left by a finished test fails the next one ("runtime dropped the
dispatch task"). The app has one runtime, so only the tests are affected.
`live_tidal_login` is the interactive device flow and reads
`SPOTATUI_TIDAL_CLIENT_ID` from the environment only; without it, it fails.

Keeping these green as you go is what keeps upstreaming cheap later.

## Later: if it is worth upstreaming

1. **Rebase** on a fresh `upstream/main` and fix whatever has drifted. Re-run
   the touch-point grep, since upstream may have added new Qobuz arms.
2. **Open the issue.** Describe Tidal support and point to the working fork
   branch. Raise:
   - whether a default client ID is acceptable (vs. config-only, or a runtime
     scrape like Qobuz);
   - the AAC fallback (or the PKCE client, if done by then);
   - one PR or two (login/browsing first, then streaming).
3. **Wait for the maintainer's answer**, and adapt the branch to it.
4. **Finish what only matters upstream:**
   - the `cd.yml` release rows and `ci.yml` legs;
   - README, `docs/`, CHANGELOG, `src/infra/tidal/AGENTS.md` (plus the
     `CLAUDE.md` stub) and its row in the root conventions table;
   - remove this file, or move what lasts into `src/infra/tidal/AGENTS.md`.
5. **Run the full check list**, including the GUI:

   ```bash
   # gui/
   npm ci && npm run lint && npm run format:check && npm run typecheck && npm test && npm run build
   ```

   Also regenerate `Cargo.lock` after any `Cargo.toml` edit, since CI runs
   with `--locked`.
6. **Follow CONTRIBUTING.md "Using AI Tools":** read the whole diff, paste
   real check output under Testing, and write the PR description yourself.
