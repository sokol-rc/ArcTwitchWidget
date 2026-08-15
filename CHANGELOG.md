# Changelog

All notable changes to ARC Live are documented here.

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
