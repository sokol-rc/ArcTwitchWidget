# Local modifications

This directory is a **vendored, locally-modified copy** of
[`pcapsql-core`](https://github.com/mtottenh/pcapsql) version **0.3.1** (as
published on crates.io). ARCTracker Sync uses it as a `path` dependency, so it is
**not** byte-for-byte identical to the published crate.

The local changes are in the TLS decryption and protocol-registration code, in
support of the traffic parsing ARCTracker Sync relies on. To see exactly which
files changed and the full diff, compare this `src/` tree against the published
`pcapsql-core 0.3.1` source (downloadable from <https://crates.io/crates/pcapsql-core>).

## Change log (ARC Live)

TLS decryption robustness for long-lived, real-world game connections:

- **TLS 1.3 KeyUpdate handled** (`tls/session.rs`, `tls/kdf.rs`,
  `stream/parsers/tls_decrypt.rs`). A post-handshake KeyUpdate now ratchets that
  direction's application traffic secret
  (`HKDF-Expand-Label(secret, "traffic upd", "", Hash.len)`), re-derives the
  key/IV, and resets the record sequence number to zero. Previously the message
  was ignored and every following record failed authentication forever.
- **TLS 1.2 encrypted Finished accounted for** (`tls/session.rs`,
  `stream/parsers/tls_decrypt.rs`). After ChangeCipherSpec, the encrypted
  handshake record consumes one AEAD sequence number per direction; without this
  the first application record decrypted at the wrong sequence number and TLS 1.2
  never decrypted at all.
- **TLS 1.2 ChaCha20-Poly1305 nonce** (`tls/decrypt.rs`, `tls/kdf.rs`). RFC 7905:
  no explicit nonce, a full 12-byte implicit IV XOR sequence number. The key
  block now derives a 12-byte IV for ChaCha suites (4 bytes stays for GCM).
- **Key-log window strand fixed** (`tls/session.rs`). `try_establish_keys` now
  returns early only when it can actually decrypt, and a TLS 1.3 session with
  only the application secrets present (handshake secrets aged out of the 4 MiB
  key-log window) goes straight to application mode instead of stranding in
  `Tls13HandshakeEncrypted` forever.
- **Reassembly gap recovery** (`stream/reassembly.rs`, `stream/manager.rs`).
  Gaps are now recorded, and `recover_stuck_gaps(threshold)` skips past a segment
  that was dropped and never retransmitted (passive `SNIFF | RECV_ONLY` capture
  never sees the retransmit), after the stream has stayed blocked for two
  maintenance ticks. Previously a single lost segment stalled a connection until
  teardown. `StreamStats` gained `bytes_skipped`.

New unit tests cover each of the above (`tls::*`, `stream::reassembly::*`).

## Licensing

The upstream library is MIT-licensed (see [`LICENSE`](LICENSE)), and that license
is retained here. The local modifications are likewise made available under the
same MIT terms.
