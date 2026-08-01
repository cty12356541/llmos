# NLOS schema fuzz targets

This package is intentionally excluded from the main Cargo workspace because
`cargo-fuzz` requires a nightly compiler and sanitizer instrumentation.

Run the bounded smoke profile from the repository root:

```sh
cargo install cargo-fuzz --version 0.13.2 --locked
rustup toolchain install nightly-2026-08-01 --profile minimal
NLOS_FUZZ_TOOLCHAIN=nightly-2026-08-01 scripts/run-fuzz-smoke.sh
```

Set `NLOS_FUZZ_SECONDS` to run each target for a time budget instead of the
default 2,000 executions. Checked-in seeds are reviewable `hex:` text; the
harness decodes marked seeds and treats mutated, unmarked input as raw bytes.
Generated corpus and crash artifacts are ignored by Git.

Targets:

- `protobuf_envelope`: bounded Protobuf parse and exact forwarding bytes;
- `canonical_body`: strict CBOR decode, critical-extension compatibility, and
  `encode(decoded) == input`;
- `signing_preimage`: authoritative domain/length parsing and exact preimage
  re-encoding.
