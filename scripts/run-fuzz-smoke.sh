#!/usr/bin/env bash
set -euo pipefail

fuzz_runs="${NLOS_FUZZ_RUNS:-2000}"
fuzz_seconds="${NLOS_FUZZ_SECONDS:-0}"
fuzz_toolchain="${NLOS_FUZZ_TOOLCHAIN:-nightly}"
fuzz_workspace="$(mktemp -d)"
trap 'rm -rf -- "$fuzz_workspace"' EXIT

verify_golden_seed() {
  local seed="$1"
  local golden="$2"
  cmp <(sed 's/^hex://' "$seed") "$golden"
}

verify_golden_seed \
  fuzz/seeds/protobuf_envelope/golden.hex \
  schema/golden/nlos.sabi.Envelope-v1.hex
verify_golden_seed \
  fuzz/seeds/canonical_body/golden.hex \
  schema/golden/nlos.canonical.DigestEnvelope-v1.hex
verify_golden_seed \
  fuzz/seeds/signing_preimage/golden.hex \
  schema/golden/nlos.canonical.DigestEnvelope-preimage-v1.hex

run_target() {
  local target="$1"
  local maximum_length="$2"
  local corpus="$fuzz_workspace/$target"
  mkdir -p "$corpus"
  cp "fuzz/seeds/$target/"*.hex "$corpus/"

  local budget=("-runs=$fuzz_runs")
  if [[ "$fuzz_seconds" != "0" ]]; then
    budget=("-max_total_time=$fuzz_seconds")
  fi

  cargo +"$fuzz_toolchain" fuzz run "$target" "$corpus" -- \
    "${budget[@]}" -max_len="$maximum_length" -timeout=5 -rss_limit_mb=2048
}

run_target protobuf_envelope 2097154
run_target canonical_body 8194
run_target signing_preimage 8402
