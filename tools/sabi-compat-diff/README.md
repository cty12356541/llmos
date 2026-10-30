# `sabi-compat-diff` — Static SABI `.proto` and Deprecation Manifest Compatibility Gate

`tools/sabi-compat-diff/sabi_compat_diff.py` is a zero-dependency (Python 3 standard library only) static contract evolution checker implementing `ADR-0014` wire-evolution discipline and `ADR-0027` (`[COMPAT-VER-001]`, `[COMPAT-VER-002]`, `[COMPAT-DEPRECATE-001]`) lifecycle state-machine invariants across `schema/nlos/sabi/v1/*.proto` and `schema/nlos/sabi/v1/deprecation-manifest.yaml`.

## Enforced Invariants

1. **No destructive `.proto` removals**:
   - Deleting an existing `.proto` file (`E_REMOVED_PROTO_FILE`), `message` (`E_REMOVED_MESSAGE`), `enum` (`E_REMOVED_ENUM`), `service` (`E_REMOVED_SERVICE`), or `rpc` (`E_REMOVED_RPC`) fails closed.
   - Deleting an existing message field or enum value fails (`E_REMOVED_FIELD_WITHOUT_RESERVED`, `E_REMOVED_ENUM_VALUE_WITHOUT_RESERVED`) unless **both** numeric (`reserved <num>;`) and identifier (`reserved "<name>";`) tombstones are recorded on the enclosing type.
2. **No in-place wire mutations or tombstone reuse**:
   - Changing an existing field's type, cardinality (`singular` / `optional` / `repeated` / `map`), `oneof` group, or field name at the same number fails (`E_CHANGED_FIELD_TYPE`, `E_CHANGED_FIELD_NAME`).
   - Dropping an existing `reserved` tombstone (`E_REMOVED_RESERVED_TOMBSTONE`) or reusing a reserved number/name (`E_RESERVED_SLOT_REUSE`) fails closed.
3. **`deprecation-manifest.yaml` lifecycle enforcement (`ADR-0027`)**:
   - Enforces monotonic `current_epoch`, `min_compatibility_window_epochs >= 2`, and `frozen: true` on all frozen channels.
   - Rejects `ACTIVE -> SUNSET` state skipping (`E_ILLEGAL_STATE_SKIP_ACTIVE_TO_SUNSET`), reverse state regressions (`E_ILLEGAL_STATE_REGRESSION`), grace windows shorter than `2` epochs (`E_INSUFFICIENT_DEPRECATION_WINDOW`), premature sunset before `earliest_sunset_epoch` (`E_PREMATURE_SUNSET`), unremoved `.proto` fields marked `SUNSET` (`E_SUNSET_FIELD_STILL_PRESENT`), and reuse of `RESERVED` / `SUNSET` symbols or field numbers (`E_MANIFEST_RESERVED_REUSE`).
4. **Permitted additive changes**:
   - Adding new `.proto` messages/enums/services, new optional `rpc` methods, new fields on fresh unreserved numbers, compliant `ACTIVE -> DEPRECATED` transitions (`earliest_sunset_epoch >= deprecated_in_epoch + 2`), and full-window `DEPRECATED -> SUNSET` retirements with `reserved` tombstones.

## Usage

Run canonical repository self-check:

```bash
python3 tools/sabi-compat-diff/sabi_compat_diff.py --self-check
python3 tools/sabi-compat-diff/sabi_compat_diff.py --self-check --json
```

Compare a baseline schema directory against a candidate schema directory:

```bash
python3 tools/sabi-compat-diff/sabi_compat_diff.py \
  --base-dir path/to/baseline/schema/nlos/sabi/v1 \
  --cand-dir path/to/candidate/schema/nlos/sabi/v1 \
  --json
```

Run the unit and subprocess test suite:

```bash
python3 -m unittest discover -s tools/sabi-compat-diff -p "test_*.py" -v
```

## Exit Codes

| Exit Code | Meaning |
| --- | --- |
| `0` | All static compatibility and deprecation lifecycle checks passed (`error_count == 0`). |
| `1` | One or more contract compatibility or lifecycle errors were detected (`error_count > 0`). |
| `2` | CLI argument usage error or malformed `.proto` / `deprecation-manifest.yaml` syntax (`E_SCHEMA_PARSE_ERROR`). |
