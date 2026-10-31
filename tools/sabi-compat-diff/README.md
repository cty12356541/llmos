# `sabi-compat-diff`

Pure Python 3 standard-library cross-epoch SABI contract evolution and deprecation-manifest compatibility checker (`W78-E3-MIGRATE-KIT`).

## Authority

- [06-架构设计总纲-v0.5.md](../../docs/design/06-架构设计总纲-v0.5.md) section 9 (`[COMPAT-VER-001]`, `[COMPAT-VER-002]`, `[COMPAT-DEPRECATE-001]`)
- [ADR-0027 Stage E SABI 长期兼容与商店治理](../../docs/management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md)
- [schema/nlos/sabi/v1/deprecation-manifest.yaml](../../schema/nlos/sabi/v1/deprecation-manifest.yaml)

## Classifications

Every channel, symbol, field, or `.proto` delta between baseline and candidate is classified into one of five buckets:

- `UNCHANGED`: Frozen wire contract and lifecycle state remain identical.
- `ADDITIVE_SAFE`: Backward-compatible extension (new frozen channel, minor/major forward bump, or new `ACTIVE` message/field/method using an untombstoned slot).
- `DEPRECATION_NOTICE`: Lawful `ACTIVE -> DEPRECATED` transition declaring `deprecated_in_epoch`, `earliest_sunset_epoch >= deprecated_in_epoch + 2`, `replacement`, and `migration_rule_id`.
- `SUNSET_RETIREMENT`: Lawful `DEPRECATED -> SUNSET` retirement after completing the two-epoch rolling grace window (or archival of an already `SUNSET` entry into permanent channel `reserved_*` tombstones).
- `BREAKING_VIOLATION`: Forbidden wire or governance regression (physical deletion without `SUNSET`, flipping `frozen: true -> false`, version regression, shortened or `< 2`-epoch grace window, premature `SUNSET`, or reuse/deletion of `reserved_field_numbers` / `reserved_symbols` tombstones).

## Usage

```bash
python tools/sabi-compat-diff/sabi_compat_diff.py \
  schema/nlos/sabi/v1/deprecation-manifest.yaml \
  schema/nlos/sabi/v1/deprecation-manifest.yaml \
  --baseline-proto-dir schema/nlos/sabi/v1 \
  --candidate-proto-dir schema/nlos/sabi/v1 \
  --json
```

### Exit Codes

- `0`: Candidate is compatible (`breaking_violation == 0`).
- `1`: Candidate contains one or more `BREAKING_VIOLATION` items.
- `2`: CLI usage error or unreadable/unparseable input file.

## Unit Tests

```bash
python -m unittest tools/sabi-compat-diff/test_sabi_compat_diff.py
```
