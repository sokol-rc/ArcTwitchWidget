# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

ARC Live - local-first Windows companion that passively decrypts ARC Raiders TLS
traffic, normalizes the game's own `POST /v1/pioneer/stats/player-v2` response,
and serves an OBS Browser Source from `127.0.0.1`. Rust 2024 cargo workspace,
Windows x64 only (non-Windows targets compile but capture stubs out).

Research status: not approved for public distribution (see `README.md` and
`docs/PRODUCTION_READINESS_RU.md`). Two hard product invariants follow from that:
capture stays **passive** (WinDivert `SNIFF | RECV_ONLY`, no reinjection), and the
app **never replays game requests or extracts bearer tokens**. Do not reintroduce
active API calls, timers against Embark hosts, or credential extraction.

## Toolchain note (this environment)

`cargo` is **not on PATH in the WSL shell** here; the Rust toolchain lives on the
Windows side (`C:\Users\Evgeniy\.cargo\bin`). It can be driven from WSL over the
UNC path, but incremental compilation cannot take its lock there, so disable it:

```bash
CARGO_INCREMENTAL=0 WSLENV=CARGO_INCREMENTAL/w /mnt/c/Users/Evgeniy/.cargo/bin/cargo.exe test --workspace
```

`WSLENV` is required - a plain `CARGO_INCREMENTAL=0` does not reach the Windows
process. PowerShell from Windows works too and is faster.

## Commands

Build and run:

```powershell
cargo build --release -p arc-live
cargo run -q -p arc-live
```

Pre-PR gate (identical to `.github/workflows/ci.yml`; the package list is
explicit so `vendor/pcapsql-core` is excluded from fmt/clippy):

```powershell
cargo fmt -p arc-live -p arc-live-capture -p arc-live-capture-service -p arc-live-collector -p arc-live-core -p arc-live-diagnostics -p arc-live-release-tool -p arc-live-storage -- --check
cargo test --workspace --locked
cargo clippy -p arc-live -p arc-live-capture -p arc-live-capture-service -p arc-live-collector -p arc-live-core -p arc-live-diagnostics -p arc-live-release-tool -p arc-live-storage --all-targets --no-deps -- -D warnings
```

Single test (tests are `#[cfg(test)] mod tests` inside each source file):

```powershell
cargo test -p arc-live-core stats::tests::normalizes_aggregate_scope_for_obs
```

Installer / portable ZIP (needs WiX 5 CLI + Bal extension):

```powershell
.\scripts\build-installer.ps1 -UnsignedDevelopmentBuild
```

Release is tag-driven (`v<version>`); `release.yml` fails unless the tag matches
the workspace `version` in `Cargo.toml`, then signs `dist/<channel>.json` with
`cargo run -p arc-live-release-tool -- sign`.

## Architecture

Data flow, one direction, crate by crate:

```
WinDivert (capture/raw.rs, dynamically loaded DLL)
  -> IPv4/TCP:443 segments (capture/packet.rs)
  -> pcapsql-core StreamManager + KeyLog TLS decryption (capture/scanner.rs)
  -> DiscoveryParser (HTTP/1 framing over the decrypted stream) -> CaptureEvent
  -> collector: CaptureEvent -> CollectorEvent (+ normalize_player_stats, json_shape)
  -> arc-live ui.rs: applies session baseline + widget-config -> AppState.overlay
  -> server.rs: /api/v1/overlay JSON + /ws broadcast -> OBS Browser Source
  -> storage: sanitized observations, stream_session, user_events (SQLite/WAL)
```

**Two-process model.** The GUI (`arc-live`) runs unprivileged; the installed
LocalSystem service `ArcLiveCapture` (`arc-live-capture-service`) owns WinDivert.
`service_client.rs` tries the service first over loopback TCP and falls back to
in-process `start_local` (portable/admin mode). The wire protocol is
newline-delimited JSON `CollectorEvent`; changing `CollectorEvent`/`ServiceRequest`
requires bumping `SERVICE_PROTOCOL_VERSION` in `crates/collector/src/lib.rs`. The
handshake additionally requires an exact `CARGO_PKG_VERSION` match, so GUI and
service must ship together. The service authenticates clients with a SHA-256
comparison against the user's `service-token` file and rejects any keylog path
that is not `…\ARC Live\…\arc-live-tls.keys`.

**TLS keys.** `session_setup.rs` reuses an `SSLKEYLOGFILE` already set in a running
Steam/Epic process, otherwise installs a user-scoped one via `setx` for the
launcher's next natural start. ARC Live never restarts the launcher or game.

**Stats normalization** (`core/src/stats.rs`) maps Embark `eventId`/`targetId`
rows onto named fields *and* keeps every numeric aggregate in
`raw_totals["event.<id>[.target.<id>]"]`. Event/target ids are experimentally
derived constants, not documented API.

**Widget indirection.** `core/src/widget_config.rs` owns the preset list
(`widget-config.json` schema 2): a free list of presets, each with 1-4 cells whose
`source`/`add`/`subtract` strings (`account.*`, `session.*`, `today.*`,
`outcome.*`, plus arbitrary `event.*` keys) resolve into numbers. `apply()` writes
every resolved preset into `OverlayStats::presets` and repairs `preset` if the
active id was deleted. Prefer exposing a new number through a `widget-config.json`
source over adding hardcoded overlay fields; the file is reread on every
successful sync and via **Перезагрузить пресеты**. Schema 1 (fixed
`account`/`session`/`outcome`/`pve`/`pvp` keys) is migrated on load and rewritten.

**Session model.** "Current stream" numbers are deltas from a baseline
(`OverlayStats::apply_session_baseline`) persisted in the `stream_session` table
and restored on restart for the same local day. Switching presets must never reset
counters; only the confirmed reset starts a new baseline.

**Overlay HTML** is embedded as `const` string literals in
`crates/arc-live/src/server.rs` (`OVERLAY_HTML`, `LIVE_OVERLAY_HTML_V2`) - single-
line minified CSS/JS, no build step, no external assets. `LIVE_OVERLAY_HTML_V2`
builds its cells from `stats.presets` at runtime, so preset count and cell count
are data, not markup. Query overrides (`preset` as id or 1-based position, `lang`,
`opacity`, `bg`, `blur`) are parsed there. There is no way to unit-test this
string from Rust; when changing it, extract the `<script>` block and at least run
`node --check` on it.

**UI** is egui/eframe, single `ArcLiveApp` in `ui.rs` (Home / Widget / Settings
pages plus onboarding), tray icon, Windows single-instance guard.

## Conventions and invariants

- **Nothing raw leaves the machine or hits disk.** Response bodies become
  `json_shape` (types only) or normalized aggregates; `sanitize_json` runs before
  every DB insert and diagnostics export; `public_state` redacts `keylog_path` and
  the internal activity log from `/api/v1/snapshot` and `/ws`.
- **Overlay contract is versioned.** Changing `OverlayStats`/`OverlaySnapshot`
  means bumping `schema_version` in `AppState::overlay_snapshot`, updating the
  assertion test in `core/src/state.rs`, and updating `docs/LOCAL_API.md`.
- **Host allowlist.** `is_allowed_host` requires the last two labels to be exactly
  `es-pio.net` (suffix-trick tests exist in `scanner.rs`); relax it only with
  matching tests.
- **Loopback only.** Both the HTTP server and the capture service bind
  `127.0.0.1` with port fallback (17842+/17843+); the selected port is persisted
  to `config.json`.
- **Bounded everything.** Decoded bodies 4 MiB, keylog window 4 MiB, connection /
  pending-request / client-random caps in `scanner.rs`, row caps in `storage`.
  Keep new buffers bounded - this process runs for a whole stream.
- **Message language split.** `AppState::record` / `tracing` messages are English
  (internal, ends up in diagnostics); `ArcLiveApp::record_user_event` and all egui
  strings are Russian (user-facing). Match the surrounding call.
- **Config files self-heal**: `AppConfig`/`WidgetConfig` `load_or_create` migrate
  missing fields, validate, and rewrite; invalid widget config falls back to
  defaults with a warning instead of failing startup.
- **`vendor/pcapsql-core`** is a locally modified MIT fork (TLS decrypt + protocol
  registration). It is excluded from fmt/clippy; record any further change in
  `vendor/pcapsql-core/LOCAL-MODIFICATIONS.md`.
- Edition 2024 idioms are used throughout (let-chains, `each_ref`); keep
  `saturating_*`/`clamp` arithmetic on counters.
- Never commit `dist/`, `target/`, `outputs/`, `*.keys`, `*.sqlite3`, signing keys,
  or `service-token` (already gitignored).
