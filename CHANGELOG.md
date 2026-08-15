# Changelog

All notable changes to ARC Live are documented here.

## 0.14.0 - 2026-08-15

- Added a capture source on the packet-capture provider that ships with Windows, so no third-party driver is needed.
- Added a second driver-free source on Windows raw sockets (SIO_RCVALL) for machines where the provider is unavailable.
- Kept WinDivert as the fallback and switched to it automatically when the raw socket delivered no inbound packets.
- Added a button that restarts Steam or Epic with the key log variable already set, so a fresh install works without a Windows reboot.
- Showed the active capture engine in the settings screen and in diagnostics.
- Added a button that rereads config.json and widget-config.json and applies the changes without a restart.
- Bumped the service protocol to 3 for the extended capture statistics.

## 0.13.1 — 2026-08-15

- Read `SSLKEYLOGFILE` from the running game and name the exact reason when statistics never arrive.
- Explained a missing or foreign key file on the Стрим screen instead of showing an endless spinner.
- Stopped reporting the launcher as ready when the key log only contains keys from other applications.
- Kept quiet when the game refuses the environment read, so a working capture is never flagged.

## 0.13.0 — 2026-08-15

- Reorganised the window around three tasks: Стрим, Виджет OBS and Настройки.
- Added an always-visible OBS bar with the active preset and a one-click switch.
- Moved the Browser Source address and appearance next to the widget preview.
- Showed the current stream counters as numbers on the Стрим screen.
- Switched the app to a dark theme that matches the OBS widget.
- Fixed status and preset markers that rendered as empty boxes in the bundled font.
- Fixed preset values that were invisible on the light background.
- Extracted the rendering layer so screens can be reviewed without running the app.

## 0.12.0 — 2026-08-15

- Replaced the five fixed widget modes with a user-defined preset list.
- Added a preset screen that shows every preset with its live values and switches OBS in one click.
- Added on-demand preset reloading and a visible reason when the preset file is rejected.
- Allowed one to four values per preset, up to twelve presets, and per-value color styles.
- Converted existing `widget-config.json` files to the preset list automatically.
- Published overlay contract schema 7 with a resolved `presets` array.
- Restored window activation when ARC Live is started a second time.

## 0.11.0 — 2026-08-14

- Added one-click PvE and PvP preset switching on the Home screen.
- Added current-stream PvE metrics for extracted loot and ARC damage.
- Added current-stream PvP metrics for player knocks and player damage.
- Added a confirmed stream-stat reset that resets both modes from one baseline.
- Automatically extends existing `widget-config.json` files with editable PvE/PvP mappings.

## 0.10.3 — 2026-08-14

- Connected stable and beta automatic updates to GitHub Releases.
- Added Ed25519 signatures for update manifests and SHA-256 verification for installers.
- Added GitHub Actions workflows for Windows CI and tagged releases.
- Migrated existing installations with an empty update feed to the public stable feed.
- Removed demonstration totals from persisted overlays created by older schemas.
- Kept widget field mappings in the user-editable `widget-config.json`.
- Reduced background work and improved shutdown behavior for long-running sessions.

## 0.10.2 — 2026-08-14

- Added the consumer installer, background capture service, recovery of the current day and stream, and compact configurable OBS widgets.
