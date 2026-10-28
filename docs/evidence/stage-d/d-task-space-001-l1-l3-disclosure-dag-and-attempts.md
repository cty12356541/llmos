# D-TASK-SPACE-001: Task Space `L1 -> L2 -> L3` Progressive Disclosure, Dagster Orthogonal `TaskPlan` DAG Override Editor, and Temporal `TaskAttempt` Racing Swimlanes Evidence (`W72-D-TASK-DAG`)

- **Wave / Lane**: `W72` / Lane 1 (`W72-D-TASK-DAG` · `D-TASK-SPACE-DAG`)
- **Assurance Level**: `H4` (Host & Frontend Deterministic Verification + Rust Integration Suite)
- **Status**: `DONE`
- **Base Commit**: `d4a9d340e0ec0a6259a7edd1191ee7f23790c1ed`
- **Authority Specs**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §19, §28.4 (`[ROAD-D-001]`, `[TASK-SPACE-001]`, `[TASK-VIS-001]`, `[TASK-REOPEN-001]`, `[RCPT-UNDO-001]`, `[RCPT-PARTIAL-001]`, `[PLAN-CHANGE-001]`, `[PLAN-OVERRIDE-001]`, `[CTRL-PLAN-001]`, `[PLAN-LAZY-001]`, `[TASK-STREAM-001]`, `[TASK-EVOLVE-001]`, `[TASK-SPEC-001]`, `[TASK-PROVIDER-001]`, `[TASK-AGENT-001]`, `[TASK-ISO-005]`, `[CTRL-GRANULAR-001]`, `[RCPT-OUTCOME-001]`)
  - [2026-10-26-stage-d-desktop-visual-and-ux-design.md](../../superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md) §2A, §3.2
  - [ADR-0025 Stage D Hybrid Desktop Compositor and Trusted UI Layering](../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)
  - [stage-d-progress.md](../../management/stage-d-progress.md) §D.3.1 #6 (`D-TASK-SPACE-DAG`), §D.3.2 (`TASK_SPACE_GAPS` 1–3 closure), §D.5.2 (`W72-D-TASK-DAG`)

---

## 1. Exclusive Write-Set Summary

1. `desktop/src/store/disclosure-model.ts` — Added `TaskSpaceLazyCursorCoordinator`, `createTaskSpaceLazyCursorCoordinator`, `clampTaskSpaceCursorLimit`, `TaskSpaceCursorSlice`, `TaskSpaceDisclosurePullPlan`, and `TaskSpacePullAuditEntry` enforcing `O(visible_rows)` `L0 -> L1 -> L2 -> L3` row-level and canvas-level cursor fences (`limit <= 64`).
2. `desktop/src/views/task-space/dag-canvas.ts` — Implemented Dagster-style Left-to-Right (`LR`) orthogonal `TaskPlan` DAG layout (`computeOrthogonalDagLayout`), `[PLAN-LAZY-001]` collapsed unmaterialized supernodes (`+N 待物化步骤 · 0 进程` / `+N Unmaterialized Steps · 0 Processes`), Statutory Lock #3 (`NODE_COMPLETED_OR_ACTIVE_LOCKED` on `COMPLETED`/`ACTIVE` nodes), and the deterministic 2-path override classifier & compiler (`BOUNDED_CONTROL_COMMAND` vs `RECOMPILE_AND_Z4_REAUTHORIZE` + Terraform `+ / ~ / -` `BlastRadiusDiff` preview).
3. `desktop/src/views/task-space/attempt-lanes.ts` — Implemented Temporal proportional `TaskAttempt` racing swimlanes (`CAS_WINNER` vs `SUPERSEDED`, `PROVIDER_ISOLATION_UNVERIFIED`), Statutory Lock #4 (`TASK_ATTEMPT_NO_MANUAL_COMMIT` / `FORBIDDEN_MANUAL_ATTEMPT_COMMIT`), `ExecutionFiber` 5-segment phase duration waterfall (`activeCpuMs`, `schedulerWaitMs`, `externalWaitMs`, `backpressureWaitMs`, `suspendedMs`), `SHARED_THREAD_LIKE` fault-domain badge, `DurableOperation` Statutory Lock #5 (`OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS`), `Cell` placement metadata, and `ReceiptAuditBar`.
4. `desktop/src/views/task-space/task-space.css` — Solid opaque `Graphite / Paper` industrial workstation stylesheet (`0` `backdrop-filter`, `0` neon glow, `0` emojis, strict `{0px, 2px, 4px, 6px}` border radii, `tabular-nums` alignment).
5. `desktop/src/views/task-space/index.ts` — Closed `TASK_SPACE_GAPS` (1–3) (`TASK_SPACE_GAPS = []`, `CLOSED_TASK_SPACE_GAPS` with `GAP-1..GAP-3`), implemented `L1` 16-state commitment table, `L2` non-modal single-row Headlamp Attribution Drawer, `L3` forensic DAG & attempt swimlanes integration, `[TASK-REOPEN-001]` terminal `retry` / `refine` reopen flows, `bindTaskSpaceToCompositor`, and preserved backward-compatible `taskSpaceView()` / `buildTaskSpaceView()`.
6. `desktop/src/views/task-space/__tests__/task-space-dag.test.ts` — 7-suite deterministic TypeScript verification test suite.
7. `desktop/src-tauri/src/plan_editor.rs` — Rust authority module implementing `PlanEditorState`, `inspect_task_plan_dag`, `expand_lazy_plan_supernode`, `preview_plan_override_diff`, `submit_plan_override`, `inspect_task_attempt_lanes`, `execute_task_attempt_control`, `reopen_terminal_task`, and their `#[tauri::command]` wrappers.
8. `desktop/src-tauri/tests/task_space_dag_override_side.rs` — 8-test Rust integration suite verifying all 6 functions, Path A vs Path B, `Z4` re-authorization enforcement, Statutory Locks #3/#4/#5, `[PLAN-LAZY-001]`, `NOT_FOUND` vs `STALE_GENERATION`, and `[TASK-REOPEN-001]`.

---

## 2. Requirement & Invariant Matrix

| Requirement / Invariant ID | Verification Target | Evidence Location | Result |
| :--- | :--- | :--- | :--- |
| **`TASK_SPACE_GAPS` (1–3) Closure** | `TASK_SPACE_GAPS.length === 0`; `CLOSED_TASK_SPACE_GAPS` documents `GAP-1` (`listTasks`/`listTaskGroups`/`listTaskNodes`/`listTaskAttempts`), `GAP-2` (`W29-A` `applicationId`/`installationGeneration`/`planRevision`), `GAP-3` (`listExecutionFibers`/`listOperations` + `NOT_FOUND` vs `STALE_GENERATION` carrying `currentGeneration`) | `desktop/src/views/task-space/index.ts`, `desktop/src-tauri/src/plan_editor.rs` | **PASS** |
| **`[ROAD-D-001]` / `[TASK-SPACE-001]` / `[TASK-VIS-001]`** | `L1` high-density commitment table covering all 16 canonical `TaskState`s, `6px` geometric SVG status glyphs, Temporal folded phase milestone summary, and Nomad 3-watermark (`Usage / Effective / Requested`) bullet micro-bar | `desktop/src/views/task-space/index.ts`, `task-space-dag.test.ts` (Suite 3) | **PASS** |
| **`[RCPT-UNDO-001]` & Statutory Lock #6** | `REVERSIBLE` (`Undo`), `COMPENSATABLE` (`Compensate`), `IRREVERSIBLE` physically omits `Undo` (`data-undo-omitted="true"`) and renders `查看操作与效果凭证` / `View Effect Proof` + `IRREVERSIBLE_NO_UNDO` badge | `desktop/src/views/task-space/index.ts`, `task-space-dag.test.ts` (Suite 3) | **PASS** |
| **`[RCPT-PARTIAL-001]`** | `L1` partial completion badge (`4 成功 / 1 失败`) and `L2` per-target breakdown table with `retry-failed-target` action | `desktop/src/views/task-space/index.ts`, `task-space-dag.test.ts` (Suites 3 & 4) | **PASS** |
| **`[TASK-REOPEN-001]`** | Terminal/partial task `retry` spawns a new `TaskAttempt` (`attempt_index + 1`, `retry_fence_epoch + 1`); `refine` spawns a new `Task` generation (`installation_generation + 1`, `plan_revision + 1`) while preserving historical receipts | `desktop/src/views/task-space/index.ts`, `desktop/src-tauri/src/plan_editor.rs`, `task_space_dag_override_side.rs` | **PASS** |
| **`O(visible_rows)` `L0 -> L1 -> L2 -> L3` Fence** | `L1` pulls only `list_tasks` (`limit <= 64`, `0` `L2`/`L3` pre-fetch); `L2` pulls only selected row's `list_task_nodes` + `inspect_resource_cost` (`0` `L3` pre-fetch); `L3` (`Ctrl/⌘+Shift+E`, `< 16ms`) pulls parent-scoped slices and on-demand supernodes (`limit <= 64`) | `desktop/src/store/disclosure-model.ts`, `task-space-dag.test.ts` (Suite 2) | **PASS** |
| **`[PLAN-LAZY-001]`** | Unmaterialized (`METADATA_ONLY` / `COLD`) subgraph super-node (`+18 待物化步骤 · 0 进程` / `+18 Unmaterialized Steps · 0 Processes`, `allocated_process_count == 0`) expands on demand in `L3` with `limit <= 64` | `desktop/src/views/task-space/dag-canvas.ts`, `desktop/src-tauri/src/plan_editor.rs` | **PASS** |
| **`[PLAN-CHANGE-001]` & Statutory Lock #3** | `COMPLETED` and `ACTIVE` `TaskNode`s are strictly read-only (`NODE_COMPLETED_OR_ACTIVE_LOCKED`); any `remove_node`, `pause_node`, `replace_node`, `merge_nodes`, `reorder_nodes` targeting them is deterministically rejected (`READ_ONLY_EXECUTED_TASK_NODE`) | `desktop/src/views/task-space/dag-canvas.ts`, `desktop/src-tauri/src/plan_editor.rs`, `task_space_dag_override_side.rs` | **PASS** |
| **`[PLAN-OVERRIDE-001]` / `[CTRL-PLAN-001]` (Path A vs Path B)** | Path A (`BoundedControlCommand`) compiles `pause_node`/`reorder_nodes` into typed `ControlScope::TaskNode` / `ControlScope::TaskPlan` commands without `Z4`; Path B (`RecompileAndZ4Reauthorize`) generates Terraform `+ / ~ / -` diff and blocks admission without a valid `Z4ReauthorizationGrant` | `desktop/src/views/task-space/dag-canvas.ts`, `desktop/src-tauri/src/plan_editor.rs`, `task_space_dag_override_side.rs` | **PASS** |
| **`[TASK-SPEC-001]` / `[TASK-PROVIDER-001]` & Statutory Lock #4** | Proportional `TaskAttempt` racing swimlanes (`CAS_WINNER` vs `SUPERSEDED`, `PROVIDER_ISOLATION_UNVERIFIED`); manual commit override deterministically rejected (`FORBIDDEN_MANUAL_ATTEMPT_COMMIT` / `TASK_ATTEMPT_NO_MANUAL_COMMIT`) | `desktop/src/views/task-space/attempt-lanes.ts`, `desktop/src-tauri/src/plan_editor.rs`, `task_space_dag_override_side.rs` | **PASS** |
| **`[TASK-AGENT-001]` / `[TASK-ISO-005]`** | `ExecutionFiber` 5-segment waterfall (`activeCpuMs`, `schedulerWaitMs`, `externalWaitMs`, `backpressureWaitMs`, `suspendedMs`) and `共享进程域 · SHARED_THREAD_LIKE` fault-domain badge | `desktop/src/views/task-space/attempt-lanes.ts`, `desktop/src-tauri/src/plan_editor.rs` | **PASS** |
| **`ANTI-AI-01..07` & `L1`/`L2` Zero-Jargon Gate** | `0` `backdrop-filter`, `0` neon glow, `0` emojis, radii strictly in `{0px, 2px, 4px, 6px}`, and `0` internal jargon (`Z1..Z4`, `TypedIntent`, `0x1F`, `Quota`, `TaskAttempt`, `Fiber`, `Cell`, `receipt_hex`, `fencing_token`, `plan_digest`) in `L1`/`L2` (`zh-CN` & `en-US`) | `task-space-dag.test.ts` (Suite 7), `compositor-wm.test.ts` (Suite 8) | **PASS** |

---

## 3. Verification Gate Outputs

### 3.1 Frontend TypeScript Verification (`task-space-dag.test.ts` & `compositor-wm.test.ts`)

```text
> npx tsx --test src/views/task-space/__tests__/task-space-dag.test.ts
[W72-D-TASK-DAG] All 7 Task Space L1->L3, DAG Override, Attempt Swimlanes & Anti-AI suites PASSED.
✔ src\views\task-space\__tests__\task-space-dag.test.ts
ℹ tests 1 | pass 1 | fail 0

> npx tsx src/compositor/__tests__/compositor-wm.test.ts
All 8 D-COMPOSITOR-WM suites passed (733 assertions).
```

### 3.2 Rust Unit & Integration Suites (`plan_editor::tests` & `task_space_dag_override_side.rs`) & Lints

```text
> rustfmt --edition 2024 --check desktop/src-tauri/src/plan_editor.rs desktop/src-tauri/tests/task_space_dag_override_side.rs
(exit 0)

> clippy-driver --edition 2024 --test -D warnings (plan_editor.rs & task_space_dag_override_side.rs)
CLIPPY_DRIVER_CLEAN_ZERO_WARNINGS

> plan_editor::tests (4 unit tests)
running 4 tests
test plan_editor::tests::unit_clamp_plan_editor_limit_enforces_max_64 ... ok
test plan_editor::tests::unit_path_a_vs_path_b_classification_is_deterministic ... ok
test plan_editor::tests::unit_statutory_lock_3_blocks_active_and_completed_nodes ... ok
test plan_editor::tests::unit_statutory_lock_4_and_5_refuse_manual_commit_and_effect_unknown_skip ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

> task_space_dag_override_side (8 integration tests)
running 8 tests
test inspect_task_attempt_lanes_and_statutory_lock_4_forbids_manual_attempt_commit ... ok
test inspect_task_plan_dag_projects_w29a_fields_and_distinguishes_not_found_vs_stale_generation ... ok
test lazy_plan_supernode_expands_on_demand_with_bounded_cursor_and_zero_preallocated_processes ... ok
test path_a_bounded_control_commands_compile_and_commit_without_z4_reauthorization ... ok
test path_b_structural_and_capability_budget_expansions_require_z4_reauthorization_and_emit_terraform_diff ... ok
test reopen_terminal_task_spawns_new_attempt_on_retry_and_new_generation_on_refine_preserving_history ... ok
test statutory_lock_3_blocks_overrides_on_completed_or_active_task_nodes ... ok
test task_space_gaps_closure_and_cursor_limit_clamp ... ok
test result: ok. 8 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

---

## 4. Residual Scope & Honest Boundaries

- `desktop/src/main.ts` and `desktop/src-tauri/src/lib.rs` remain untouched by `W72-D-TASK-DAG` per the strict exclusive write-set boundary (`pub mod plan_editor;` was pre-exported in `lib.rs` and `taskSpaceView()` was pre-registered in `main.ts`).
- Full cross-surface end-to-end integration across all 6 `W72` lanes is gated at the post-barrier wave integration step (`W73`).
