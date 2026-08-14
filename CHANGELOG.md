# Changelog

All notable changes to ARC Live are documented here.

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
