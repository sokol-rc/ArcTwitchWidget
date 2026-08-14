# Contributing

ARC Live targets Windows and uses Rust 2024. Keep changes local-first: no game
credentials, raw API values, or TLS key material may be persisted or exposed by
the localhost API.

Before opening a pull request, run:

```powershell
cargo fmt -p arc-live -p arc-live-capture -p arc-live-capture-service -p arc-live-collector -p arc-live-core -p arc-live-diagnostics -p arc-live-release-tool -p arc-live-storage -- --check
cargo test --workspace --locked
cargo clippy -p arc-live -p arc-live-capture -p arc-live-capture-service -p arc-live-collector -p arc-live-core -p arc-live-diagnostics -p arc-live-release-tool -p arc-live-storage --all-targets --no-deps -- -D warnings
```

Do not commit `dist`, `target`, diagnostics, databases, keylogs, signing keys,
or locally generated service tokens. Public releases are created from a
`v<version>` tag by the release workflow.
