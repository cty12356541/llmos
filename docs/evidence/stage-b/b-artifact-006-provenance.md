# B-ARTIFACT-006：Per-revision Provenance 最小前缀

> 状态：`PARTIAL_PASS`
>
> 日期：2026-09-05
>
> 基线：HEAD `5c8f2de`；前序证据 `b-artifact-001`（§Windows fsync）、`b-artifact-002`（staged publication）、`b-artifact-005`（retention）

## 1. 验收目标

关闭 B-ARTIFACT 未决项「provenance」最小前缀：每条 committed revision 绑定一条
immutable provenance receipt（source triple），来源为 caller-asserted opaque
（`put_revision`）或 owner-derived（`publish_staged_revision` 自 publication
receipt 复制 task/permit/write-set triple）。`get_revision` 在无 provenance 时
fail-closed（`ProvenanceIncomplete`）；audit 平面（`inspect_revision`、
`inspect_provenance`、`list_revisions`、package verify）不 gate。

不在本前缀范围内：lineage 链、attestation 验证、TaskWriteSet consumer 接线、
pre-v7 revision 回填、legal hold。

## 2. 实现要点

- schema v7：`artifact_provenance_receipts` immutable 表 + trigger；
- `ProvenanceSourceTriple` / `ProvenanceSourceKind` / `ArtifactProvenanceReceipt`；
- `insert_caller_asserted_provenance` / `insert_owner_derived_provenance` 于
  `put_revision` / `commit_publication` 同事务写入；
- owner-derived readback 交叉校验 publication receipt 绑定；
- `PutRevisionRequest` 增 `provenance` 字段（breaking 于测试 harness，非 wire）。

## 3. 验证证据

新增 `crates/nlos-artifact/src/provenance.rs`、`tests/provenance.rs` 6 项：

| 测试 | 断言要点 |
| --- | --- |
| `put_records_caller_asserted_provenance_and_reads_require_it` | triple 持久化 + byte read 需 provenance |
| `put_replay_preserves_provenance_and_does_not_duplicate_rows` | replay 单行 + triple 不变 |
| `publish_records_owner_derived_provenance_bound_to_publication_receipt` | OwnerDerived + publication 绑定 |
| `staged_publication_replay_does_not_duplicate_provenance` | replay 不重复 provenance 行 |
| `provenance_receipt_is_immutable_and_survives_restart` | trigger + reopen |
| `revision_without_provenance_fails_closed_on_byte_read_but_metadata_inspectable` | fail-closed gate |

既有测试 harness 补 `provenance` 字段（happy_path、retention、staged_publication、
support）；`nlos-application` test-support 同步（artifact API 变更）。

本地验收（实跑）：

| 命令 | 结果 |
| --- | --- |
| `cargo test -p nlos-artifact` | PASS：全 crate 测试绿 |
| `cargo clippy -p nlos-artifact --all-targets -- -D warnings` | PASS |

## 4. 已知限制

- pre-v7 已有 revision 无 provenance，byte read fail-closed 直至专用 repair 切片；
- caller-asserted triple 不验语义（占位 discipline）；
- 无跨 revision lineage / attestation / TaskWriteSet 消费；
- Windows 实机 fsync 与 CI workspace 全仓门未在本切片后复跑。
