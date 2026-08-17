# ARC Live

## Disclaimer

**ARC Live is an independent hobby project. It is not affiliated with,
endorsed by, or supported by Embark Studios. ARC Raiders, its name and its
materials are trademarks and copyrights of Embark Studios and its licensors.**

This is a research build. To produce statistics, ARC Live decrypts and reads
the game's own network responses on the local machine. The game's terms of
service prohibit interception and analysis of its network protocol, and the
anti-cheat may treat any third-party software running alongside the game as a
violation.

**Using ARC Live may therefore get your account banned — temporarily or
permanently, up to losing everything you bought and played for. Nobody can
promise otherwise.**

The software is provided as is, without any warranty. The author accepts no
liability for account bans, lost progress, or any other damage. You use it at
your own risk. If the account matters to you, do not use ARC Live on it.

What ARC Live does not do:

- it does not modify game files and does not read game memory;
- it does not automate input and gives no advantage in combat;
- it only ever looks at traffic on this machine, and only at the game's own
  responses;
- it does not send statistics, keys, or tokens anywhere;
- it does not call Embark servers on its own behalf.

See the production audit in
[`docs/PRODUCTION_READINESS_RU.md`](docs/PRODUCTION_READINESS_RU.md) for the
full legal and technical assessment.

[![CI](https://github.com/sokol-rc/ArcTwitchWidget/actions/workflows/ci.yml/badge.svg)](https://github.com/sokol-rc/ArcTwitchWidget/actions/workflows/ci.yml)
[![Release](https://github.com/sokol-rc/ArcTwitchWidget/actions/workflows/release.yml/badge.svg)](https://github.com/sokol-rc/ArcTwitchWidget/actions/workflows/release.yml)
[![Latest release](https://img.shields.io/github/v/release/sokol-rc/ArcTwitchWidget?display_name=tag)](https://github.com/sokol-rc/ArcTwitchWidget/releases/latest)

ARC Live is a local-first Windows companion for discovering ARC Raiders raid
telemetry and rendering normalized statistics in OBS Browser Sources.

The current milestone is the **0.13 development build**. It configures a private TLS
key log, exposes a localhost-only status/overlay server, stores sanitized
observations in SQLite, and can export a diagnostic bundle that excludes TLS
secrets. When the game returns to Speranza and requests player statistics, ARC
Live reads that native response once and updates OBS over the local WebSocket.
ARC Live no longer extracts bearer tokens or repeats the game's API request.

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
WinDivert capture. The GUI receives only sanitized observations and normalized
totals.
The portable ZIP does not install this service and must be started manually
with **Run as administrator**.

```powershell
.\target\release\arc-live.exe
```

For normal use, run the single `ARC-Live-Setup-<version>.exe`. The ZIP is an
advanced portable fallback only. Do not move only the portable EXE away from
its two WinDivert runtime files.

The preferred local health endpoint is `http://127.0.0.1:17842/api/v1/health`.
The live OBS source is `http://127.0.0.1:17842/overlay/live`; its stable JSON
contract is available at `/api/v1/overlay`. If that port is occupied, ARC Live
selects a free loopback port, saves it, and shows the resulting URL in the app.
Use **Show demo data** in the app
to lay out an OBS scene without launching the game.

Widget presets are a user-editable list. ARC Live ships five (account totals,
current stream, Win/Lose, fast PvE, fast PvP), and `widget-config.json` can add,
rename, reorder or delete presets and choose from one to four values in each of
them. The **Виджет OBS** screen lists every preset with its live values next to a
preview of the source, and **Перезагрузить пресеты** picks up file edits without
restarting. A bar at the bottom of the window always shows which preset is on air
and switches it from any screen. Switching a preset never resets the
stream counters. An OBS source can override the selection with `?preset=<id>` or
`?preset=<position>`. The app also switches the widget between Russian
and English and controls its background opacity. Individual Browser Sources can
override those settings with `?lang=ru|en`, `?opacity=0..100`, `?bg=RRGGBB`,
and `?blur=0..20`. The readable headerless Browser Source size is `700 × 80`.
The widget scales itself to whatever size the Browser Source is given, drawing
text at the final resolution, so a bigger widget comes from resizing the source
(for example `1400 × 160`) rather than stretching it in the scene. `?scale=`
sets the factor manually and `?fit=off` restores the unscaled behaviour.
The app provides Transparent, Smoke, Glass, and Solid background presets plus a
custom color picker, opacity slider, and blur slider. Session balance is
rendered with an explicit green plus or red minus.

The current stream baseline and latest calculated overlay are saved to SQLite
after every native game statistics response. If ARC Live, ARC Raiders, Steam, or Windows is
restarted, the app restores today's stream before reconnecting and continues
from the same counters. The Home screen shows a short user-facing event history
for the current day. `Сбросить статистику стрима` resets the stream
counters of every preset from one new baseline and requires confirmation. Switching presets
never resets the counters.

Every numeric `(eventId, targetId, amount)` aggregate returned by the player
statistics endpoint is now retained in the local overlay snapshot under
`raw_totals`; corresponding stream and daily deltas are exposed separately.
The user-owned `%LOCALAPPDATA%\ArcLive\ARC Live\data\widget-config.json`
defines the preset list itself: which values, labels and styles each preset
shows. An adjacent configuration from an older/portable build is migrated
automatically, and the pre-0.12 fixed layout is converted to the preset list on
first start. ARC Live rereads the file on every successful sync and on demand,
so preset changes no longer require a new executable. See
[`docs/WIDGET_CONFIG_RU.md`](docs/WIDGET_CONFIG_RU.md).

ARC Live automatically reuses TLS capture already enabled in a running Steam or
Epic process. If neither launcher is configured, it installs a user-scoped
`SSLKEYLOGFILE` path once for future normal launcher starts. ARC Live never
closes or restarts the launcher or game; on the first setup only, the setting
becomes active the next time the launcher starts naturally.

ARC Live recognizes regional `*.es-pio.net` API hosts from the game's own TLS
SNI and HTTP Host values. It passively reads the native
`POST /v1/pioneer/stats/player-v2` response sent when the player returns to
Speranza. There is no timer, request replay, or extra call to Embark. Raw response
values are never persisted. Only the JSON field/type shape and normalized
aggregate totals are stored or published locally. The versioned
`/api/v1/overlay` contract (schema 7) exposes the resolved preset list plus
named counters such as ARC eliminations, downs, revives, damage, duration, and
XP.

On first run, ARC Live creates `%LOCALAPPDATA%\ARC Live\config.json`. The local
port, overlay dimensions, and candidate ARC Raiders process names can be
adjusted there.

For the first real ARC Raiders capture, follow
[`docs/FIRST_LIVE_TEST_RU.md`](docs/FIRST_LIVE_TEST_RU.md).

## Security boundary

- The HTTP server binds to loopback only.
- Bearer tokens are not extracted; TLS secrets are never written to logs or
  diagnostic bundles.
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

The earlier 0.10.3 release was withdrawn. A new installer will be published
only after the dynamic-region and lobby-event flow passes a live raid test.
[GitHub Releases](https://github.com/sokol-rc/ArcTwitchWidget/releases) remains
the update source once that validated build is available.

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

## License

ARC Live is source-available under the
[PolyForm Noncommercial License 1.0.0](LICENSE).

- **Noncommercial use is free.** Personal use, hobby projects, streaming for
  yourself, research and education are permitted at no charge.
- **Commercial use requires a separate license** from the copyright holder.
- This is a source-available license, not an OSI-approved "open source" license,
  because it restricts commercial use.

Releases up to and including 0.16.0 were published under the MIT License and
stay available under those terms; this license applies from 0.17.0 onwards.

The vendored `vendor/pcapsql-core` library is third-party software under the MIT
License. Other dependencies retain their own licenses. See
[`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).
