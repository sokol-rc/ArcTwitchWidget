# ARC Live

> **Research status — not approved for public distribution.** This build relies
> on passive packet inspection and replay of an internal ARC Raiders endpoint.
> Embark has not authorized this integration. Do not distribute or use it on a
> production account until an approved data source or written permission is
> available.

See the current production audit in
[`docs/PRODUCTION_READINESS_RU.md`](docs/PRODUCTION_READINESS_RU.md).

[![CI](https://github.com/sokol-rc/ArcTwitchWidget/actions/workflows/ci.yml/badge.svg)](https://github.com/sokol-rc/ArcTwitchWidget/actions/workflows/ci.yml)
[![Release](https://github.com/sokol-rc/ArcTwitchWidget/actions/workflows/release.yml/badge.svg)](https://github.com/sokol-rc/ArcTwitchWidget/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/sokol-rc/ArcTwitchWidget?display_name=tag)](https://github.com/sokol-rc/ArcTwitchWidget/releases/latest)

ARC Live is a local-first Windows companion for discovering ARC Raiders raid
telemetry and rendering normalized statistics in OBS Browser Sources.

The current milestone is the **Consumer Installer 0.10.3 build**. It configures a private TLS
key log, exposes a localhost-only status/overlay server, stores sanitized
observations in SQLite, and can export a diagnostic bundle that excludes bearer
tokens and TLS secrets. Normalized player totals start refreshing automatically
every 15 seconds and update OBS over the local WebSocket without restarting the
launcher or game.

## Build

```powershell
cargo test --workspace
cargo build --release -p arc-live
```

The executable is written to `target\release\arc-live.exe`. A distributable
folder must also contain the official `WinDivert.dll` and `WinDivert64.sys`
files shipped in the release ZIP.

## Run

The installed ARC Live GUI runs as a normal user. A small `ArcLiveCapture`
Windows service, installed once with Administrator approval, owns the passive
WinDivert capture. Captured credentials and request context remain inside that
process; the GUI receives only sanitized observations and normalized totals.
The portable ZIP does not install this service and must be started manually
with **Run as administrator**.

```powershell
.\target\release\arc-live.exe
```

For normal use, run the single `ARC-Live-Setup-<version>.exe`. The ZIP is an
advanced portable fallback only. Do not move only the portable EXE away from
its two WinDivert runtime files.

The default local health endpoint is `http://127.0.0.1:17842/api/v1/health`.
The live OBS source is `http://127.0.0.1:17842/overlay/live`; its stable JSON
contract is available at `/api/v1/overlay`. Use **Show demo data** in the app
to lay out an OBS scene without launching the game.

The widget has five presets selectable in ARC Live: fast PvE (stream loot and
ARC damage), fast PvP (player knocks and player damage), lifetime account
totals, full current-stream statistics, and a compact Win/Lose counter. The two
fast buttons on the Home screen switch the existing OBS Browser Source live.
An OBS source can override the selected preset with `?preset=1..5`. The app also switches the widget between Russian
and English and controls its background opacity. Individual Browser Sources can
override those settings with `?lang=ru|en`, `?opacity=0..100`, `?bg=RRGGBB`,
and `?blur=0..20`. The readable headerless Browser Source size is `700 × 80`.
The app provides Transparent, Smoke, Glass, and Solid background presets plus a
custom color picker, opacity slider, and blur slider. Session balance is
rendered with an explicit green plus or red minus.

The current stream baseline and latest calculated overlay are saved to SQLite
after every successful refresh. If ARC Live, ARC Raiders, Steam, or Windows is
restarted, the app restores today's stream before reconnecting and continues
from the same counters. The Home screen shows a short user-facing event history
for the current day. `Сбросить статистику стрима` resets all PvE/PvP stream
counters from one new baseline and requires confirmation. Switching presets
never resets the counters.

Every numeric `(eventId, targetId, amount)` aggregate returned by the player
statistics endpoint is now retained in the local overlay snapshot under
`raw_totals`; corresponding stream and daily deltas are exposed separately.
The user-owned `%LOCALAPPDATA%\ArcLive\ARC Live\data\widget-config.json`
selects which values and labels are shown in each widget slot. An adjacent
configuration from an older/portable build is migrated automatically. ARC Live rereads the file on every successful sync,
so field-mapping corrections no longer require a new executable. See
[`docs/WIDGET_CONFIG_RU.md`](docs/WIDGET_CONFIG_RU.md).

ARC Live automatically reuses TLS capture already enabled in a running Steam or
Epic process. If neither launcher is configured, it installs a user-scoped
`SSLKEYLOGFILE` path once for future normal launcher starts. ARC Live never
closes or restarts the launcher or game; on the first setup only, the setting
becomes active the next time the launcher starts naturally.

After the in-memory Embark token and the game's request context are observed,
ARC Live automatically repeats the game's exact read-only
`POST /v1/pioneer/stats/player-v2` request every 15 seconds. Request
headers, body values, raw response values, and credentials are never persisted.
Only the JSON field/type shape and normalized aggregate totals are stored or
published locally. The versioned `/api/v1/overlay` contract exposes both the five
default overlay totals and additional known counters such as ARC eliminations,
downs, revives, damage, duration, and XP.

On first run, ARC Live creates `%LOCALAPPDATA%\ARC Live\config.json`. The local
port, overlay dimensions, and candidate ARC Raiders process names can be
adjusted there.

For the first real ARC Raiders capture, follow
[`docs/FIRST_LIVE_TEST_RU.md`](docs/FIRST_LIVE_TEST_RU.md).

## Security boundary

- The HTTP server binds to loopback only.
- Bearer tokens and TLS secrets are never written to logs or diagnostic bundles.
- The user-scoped TLS key log is retained locally so ARC Live can attach after
  the game starts; it is stored under `%LOCALAPPDATA%\ARC Live` and never exported.
- The app does not read game memory, inject code, or automate input.
- WinDivert runs in passive `SNIFF | RECV_ONLY` mode: matching packets are
  copied to ARC Live and are not blocked, changed, or reinjected.
- Compressed (`gzip`, `deflate`, Brotli) and chunked API responses are decoded
  with a strict 4 MiB safety limit.
- `vendor/pcapsql-core` is MIT-licensed; its notice is retained in the vendor
  directory.

## Consumer release

Download the current installer from
[GitHub Releases](https://github.com/sokol-rc/ArcTwitchWidget/releases/latest).
Version 0.10.3 is the first build connected to the public update channel, so it
must be installed manually once. Later stable releases are discovered and
installed from inside ARC Live.

The update channels have permanent addresses:

- stable: `https://github.com/sokol-rc/ArcTwitchWidget/releases/latest/download/stable.json`;
- beta: `https://github.com/sokol-rc/ArcTwitchWidget/releases/download/beta/beta.json`.

Every update manifest is signed with the ARC Live Ed25519 release key embedded
in the application. ARC Live verifies that signature, the installer size, and
its SHA-256 hash before launching it. GitHub HTTPS is therefore not the only
integrity boundary.

Build an unsigned development installer with:

```powershell
.\scripts\build-installer.ps1 -UnsignedDevelopmentBuild
```

Tagged versions are built and published by GitHub Actions. The current public
installer does not yet have a commercial Windows Authenticode certificate, so
Windows SmartScreen can show an unknown-publisher warning. Authenticode can be
added later as a second trust layer without changing the update protocol. See
[`docs/INSTALLATION_RELEASE_RU.md`](docs/INSTALLATION_RELEASE_RU.md).
