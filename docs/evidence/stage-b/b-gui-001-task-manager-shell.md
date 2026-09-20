# B-GUI-001:可信 Tauri 任务管理器壳(W32-A 只读半 + W32-B 写入半 + W32-C parity 钉死 + W32-D 权限/预算可见 + W32-E 资源监控 + W32-F 应用表面呈现)

> 状态:`PARTIAL PASS`(读半 W32-A + 写入半 W32-B + parity 钉死 W32-C + 权限/预算可见 W32-D + 资源监控 W32-E + 应用表面呈现 W32-F 已落地)
>
> 日期:2026-09-20(W32-A)/ 2026-09-21(§W32-B、§W32-C、§W32-D、§W32-E、§W32-F)
>
> 对应:`ROAD-B-005`/B5-4 读半边、`ROAD-B-002`/B2-2 UI Surface 维度(§W32-F)、`[SABI-AUTH-001]`、`[CTRL-PARITY-001]`、[ADR-0011](../../management/adrs/0011-ipc-principal-auth-signature-passthrough.md)、[B-TASK-006L](./b-task-006l-system-control-recovery-handler.md)、[B-SCHEMA-006](./b-schema-006-typescript-python-ipc-clients.md)

## 1. 实现范围

新增独立 `desktop/` 目录(Tauri 2 + TypeScript 前端 + Rust 后端命令层),不触碰根 `Cargo.toml` workspace members(`src-tauri/Cargo.toml` 自带空 `[workspace]`,是独立 workspace 根;对 `crates/` 的依赖只以相对 path dep 出现在其中):

1. **IPC 客户端接线(唯一入口,无明文捷径)**:后端命令层 `desktop/src-tauri/src/ipc.rs` 的每条只读命令都经 `nlos_system_control::auth::dispatch_over_authenticated_socket`——ADR-0011 challenge-response 认证入口——完成真实 dispatch;principal/密钥三要素来自环境变量或 GUI 会话配置,Ed25519 种子在派发时从 operator 提供的密钥文件读取(0600),进程内不缓存、不入仓库。命令因上游 future 持有非 `Sync` 的 `&dyn ProcessInspector` 参数而非 `Send`,以同步命令 + `tauri::async_runtime::block_on` 收口(注释在案)。
2. **类型化错误面**:全部命令返回 `DesktopError { code, message }`(`CONFIG`/`HANDSHAKE`/`IPC`/`CONTROL`/`UNSUPPORTED_PLATFORM`/`INTERNAL`),由 `ControlError` 变体稳定映射;后端命令零 `unwrap`。
3. **读侧视图**:恢复总览(InspectHealth)、语义恢复(InspectSemanticHealth)、任务查询(InspectTask)、进程查询(InspectProcess)、资源查询(InspectResource)、指标导出(ExportMetrics/ExportSemanticMetrics)渲染 SABI Receipt 数据;`desktop/src-tauri/src/dto.rs` 是 `ControlReceipt` → JSON DTO 的单一投影点,mutation 结果形态显式单独枚举(`unexpected_mutation`),不伪造失败。**命令面无任何 mutation 派发路径**(W32-B)。
4. **一致性自检模式**:`parity_check` 命令把同一只读命令派发两次——GUI 经认证入口,真实 `system-control-cli` 二进制经 plain 入口——比对两侧 `ControlReceipt::to_bytes` hex(CLI stdout 首行 `RECEIPT <hex>`);每张回执页同时显示 `receipt_hex` 供人工比对。与 B-TASK-006L 已固化的三入口字节一致契约同源。
5. **开发夹具**(feature `dev-fixture`):`devfixture.rs` + `examples/dev_server.rs` 在临时目录装配真实权威(真 Ed25519 `IdentityAuthority`、真系统墙钟 `AuthorityClock`、含 escalated 计划的 `SqliteTaskAuthority`),同时开放认证入口(`authenticated_serve_one_control`)与 plain 入口(`serve_one` + `handle_for_ipc`,与 `control_ipc_auth.rs`/`control_command_cli.rs` harness 同形态),密钥随机生成(/dev/urandom 种子)只写 0600 临时文件。

## 2. 验证(全部本机实跑,macOS/darwin arm64,rustc 1.97.1,Node 26.3.0)

- `desktop/`:`npm install` → `npm run build`(tsc 严格模式 + vite 7 build)通过,产物 `dist/`。
- `desktop/src-tauri/`:`cargo check`、`cargo build`(debug,含 GUI 二进制链接)通过;tauri 2 依赖树全量拉取编译。
- `cargo clippy --all-targets --features dev-fixture -- -D warnings`:0 warning;`cargo clippy --all-targets -- -D warnings`(默认 feature):通过;`cargo fmt --check`:通过。
- `cargo test --features dev-fixture`:**4 项集成测试全过 + 3 项单元测试全过**:
  - `authenticated_inspect_health_matches_plain_entry_bytes`:GUI 后端经认证入口的真实 dispatch 回执(worker `BackingOff`、`durable_escalated=1`、一条 escalated 告警),且与同命令 plain 入口(`dispatch_over_socket`)receipt **字节一致**;
  - `inspect_task_by_plan_id_round_trips_the_escalated_alert`;
  - `unwired_inspectors_surface_typed_not_found_failures`:认证 GET 信封仍完整穿越,失败是回执内类型化 `NOT_FOUND`(inspector 未接线),与 CLI 行为一致;
  - `missing_endpoint_maps_to_typed_handshake_error`:不存在端点 → `DesktopError{code: HANDSHAKE}`。
- 真实 CLI 二进制 parity 探针(#[ignore],`LLMOS_SYSTEM_CONTROL_CLI=…/target/debug/system-control-cli cargo test … -- --ignored`):**通过**——GUI 认证入口 receipt hex 与真实 `system-control-cli` 进程(plain 入口)stdout 的 `RECEIPT` hex 逐字节相等,CLI exit 0。探针以 `spawn_blocking` 运行子进程(current_thread 测试运行时不能被同步 `output()` 阻塞,否则 plain accept 循环停摆——已注释在案)。
- 手动演示链路实跑:`cargo build -p nlos-system-control`(根 workspace,产物仅入 gitignored target/)→ `dev_server` 启动打印全部连接参数 → `system-control-cli <plain_socket> inspect-health` 输出 `RECEIPT c0c0…671…` + `outcome=inspected worker_state=BackingOff … durable_escalated=1 … alerts=1`(exit 0)。GUI 二进制以同套环境变量启动,窗口正常注册(1180×800),进程零 stderr 错误后正常退出。
- 未运行/未验证:完整 `tauri build` 打包(bundler/签名;`bundle.active=false`,README 记录);GUI 窗口内交互式点按(本 Agent 会话无 Accessibility/Screen Recording 权限,以窗口注册 + 零错误日志 + 命令层集成测试替代,README 演示步骤可供人工复验);Windows 面(认证入口上游仅 Unix)。

## 3. 当前能证明什么

- 桌面壳的每条 inspect 命令都走真实认证 IPC 入口完成挑战应答与 typed 交换,gate「真实 IPC 走 authenticated 入口」成立;
- 认证入口读回的 inspect 数据与 plain 入口、真实 CLI 二进制的 receipt 字节一致,gate「inspect 数据与 CLI 一致」在 InspectHealth/InspectTask/未接线 NotFound 形态上有活体证据;
- 失败面全类型化(握手拒绝、传输、契约、配置、未接线),无 panic 路径;
- 密钥零入库、零缓存,连接配置不落盘。

## 4. 当前不能证明什么(W32-B/C 与后续)

1. **写入半(W32-B)**:ack/resume/pause/cancel/kill/throttle/reclaim 等控制动作的 GUI 授权派发与 Receipt 展示——本壳命令面刻意不含 mutation。
2. **parity 钉死(W32-C)**:GUI↔NL↔CLI 三路径逐字节相等需下沉为仓库级测试矩阵(成功/NotFound/Rights 三形态);本目录的 `authenticated_inspect_health_matches_plain_entry_bytes` 与 ignore 探针是最小前驱。
3. **process/resource inspector 生产接线**:客户端 inspector 传 `None`(与 CLI 相同),回执为类型化「not wired」失败;接宿主 `ProcessAuthority`/`ResourceAuthority` 属后续接线。
4. **Windows 认证入口、bundle/图标/签名、多窗口**未做(上游认证入口仅 Unix;`bundle.active=false`)。
5. dev 夹具的 nonce 源为 /dev/urandom 种子的 splitmix(开发演示;生产接线必须 OS 级 RNG——上游设施已注明)。

## 5. 工件清单

- `desktop/`:Tauri 2 壳(前端 `src/`,后端 `src-tauri/src/{lib,ipc,dto,error,devfixture}.rs`,`examples/dev_server.rs`,`tests/authenticated_read_side.rs`,`README.md` 运行/演示/parity 说明)。
- 本证据文件(唯一 desktop/ 外写集工件)。

## §W32-B 写入半(2026-09-21)

### W32-B.1 实现范围

gate:「pause/cancel/kill/throttle/reclaim 真实下发 ControlCommand」+ Receipt 展示 + 破坏性动作确认 + 写路径 parity 探针。

1. **授权动作面(单一入口)**:后端 `submit_control` 命令(`ipc.rs`)接收类型化 `ControlAction`(11 个动作,serde tag 与 CLI operation 名一致:ack-recovery-alert / ack-semantic-recovery-alert / resume-semantic-recovery / ack-resource-recovery-alert / resume-resource-recovery / pause-operation / resume-operation / cancel-operation / kill-operation / throttle-operation / reclaim-operation),经单一编译点 `build_control_command` 构造真实 `ControlCommand`——§25.3 命令身份每次派发由 /dev/urandom 新生成 16 字节;派发前类型化校验(32 hex 目标、非空 reason、throttle 1..=100 镜像上游 wire 前拒绝规则);随后与读命令共用同一 `dispatch_control`(原 `dispatch_read` 更名,认证入口不变:`dispatch_over_authenticated_socket`,无任何新控制路径)。CAS 预期由前端从 inspect 状态带入:恢复告警动作取告警行 `total_failures`(「控制动作」页先巡检读取告警再按行下发),操作控制取目标 generation/revision(进程可先经「进程查询」读取;inspector 未接线前手填)。
2. **Receipt 类型化展示**:`dto.rs` 把 W32-A 的 `unexpected_mutation` 占位形态替换为 8 个第一类回执形态(acknowledged / resumed / operation_paused / operation_resumed / operation_cancelled / operation_killed / operation_throttled / operation_reclaimed,各携带权威 `receipt_id_hex`);match 穷尽无通配臂。前端按动作渲染标题 + `receipt_reference`,回执页脚恒显 `control_command_id`/`correlation_id`/`receipt_hex`;类型化失败(SabiFailure)红色卡片、mutation 成功绿色卡片,与读侧巡检视觉区分,无原始错误倾倒。
3. **kill 两步确认**:kill-operation 首次点击仅武装(5 秒窗口、按钮变红、行内提示),再次点击才真正下发;纯应用内 DOM 状态,无新控制路径、无系统弹窗。
4. **写路径 parity 探针**:`parity_check_write` 把 W32-A 自检方法扩展到一条写路径命令(pause-operation):每次运行生成新命令 id,GUI(认证入口)与真实 `system-control-cli`(plain 入口)以字节同一的命令各派发一次,比对 receipt hex;读路径 `parity_check` 与 CLI 子进程运行收口为共享 `run_cli` 投影。完整三形态矩阵钉死仍属 W32-C。
5. **顺带修复(写集内必要债)**:W32-A 的 `dto.rs` outcome match 落后于 SABI v1.3/v1.4 命令面(基线 375000f 上 `cargo check` 即 E0004:`ResourceRecoveryInspected`/`OperationKilled`/`OperationThrottled`/`OperationReclaimed` 未覆盖)——W32-B 以显式 DTO 形态补齐穷尽匹配;`devfixture.rs` 同步补齐 `TaskSpec`(application_id/plan_revision)与 tri-domain `RecoveryWorkerHealth` 字段。另补接线 `inspect_resource_health` 读命令(G8 资源域动作的 CAS 预期来源;W32-A 读面未覆盖该域)。

### W32-B.2 验证(本机实跑,macOS/darwin arm64)

- `desktop/`:`npm install` → `npm run build`(tsc 严格 + vite 7)通过;**`npm run tauri build` 通过(EXIT=0,295 crate release 编译,产物 `src-tauri/target/release/llmos-desktop`;`bundle.active=false`,不含打包/签名)。
- `desktop/src-tauri/`:`cargo fmt --check` 通过;`cargo clippy --all-targets --features dev-fixture -- -D warnings` 与默认 feature 两态均 0 warning;`cargo test --features dev-fixture` **14 项全过**(6 单元 + 4 读侧 + 4 写侧):
  - `ack_recovery_alert_mutates_authority_and_echoes_receipt_reference`:经认证入口 ack 夹具 escalated 告警 → `acknowledged` 回执(命令 id `d1…` 回显、receipt 引用非空),复检 InspectHealth `durable_unacknowledged_escalated=0` 且告警行携带同一回执引用——**真实 TaskAuthority CAS mutation**;
  - `cas_mismatch_surfaces_typed_conflict_failure_receipt`:CAS 预期 999 → 回执内类型化 `CONFLICT`/`RETRY_DIRECTIVE_DO_NOT_RETRY` 失败(派发本身成功穿越);
  - `operation_commands_dispatch_real_submits_with_typed_unwired_failures`:pause/kill/throttle/reclaim 四形态真实 submit 信封穿越认证入口,夹具未接线 executor → 确定性类型化 `NOT_FOUND`(「operation control execution backend is not wired」)失败回执;同字节 pause 命令经认证入口与 plain 入口(CLI 同路)失败回执**逐字节相等**(写路径 parity 前驱,读侧 B-TASK-006L 契约同源);
  - `semantic_and_resource_arms_route_to_their_own_ledgers`:同一 plan_id 在 semantic/resource ledger → 各自类型化 `NOT_FOUND`(域路由不串)。
- 真实 CLI 活体冒烟(读侧 ignore 探针 + 写侧手动):`LLMOS_SYSTEM_CONTROL_CLI=…/target/debug/system-control-cli cargo test … -- --ignored` 通过(真实二进制 plain 入口 receipt hex 与 GUI 认证入口逐字节相等);dev_server 夹具 + 真实 CLI 连跑两次同参数 pause-operation,RECEIPT hex 完全一致(确定性失败回执字节可比),`outcome=failure code=3 … backend is not wired`。
- 未运行/未验证:GUI 窗口内交互式点按(同 W32-A 限制,以命令层集成测试 + 窗口注册替代;README 演示步骤供人工复验);操作执行 executor 的宿主接线(后续波次,未接线面的回执形态已钉)。

### W32-B.3 边界与遗留

1. **W32-C**:GUI↔NL↔CLI 三路径逐字节相等矩阵(成功/NotFound/Rights 三形态 + 写路径多形态)下沉为仓库级测试;本波次的写侧字节一致断言与 `parity_check_write` 探针是最小前驱。
2. **executor 宿主接线**:pause/resume/cancel/kill/throttle/reclaim 的真实执行(`OperationCommandExecutor` 三执行器:process-kill / resource-throttle / working-set-reclaim)由宿主 NLOS 进程接线;桌面壳派发面完整,夹具上回执为类型化未接线失败(与 CLI 同形态,parity 不破坏)。
3. **process/resource inspector 未接线**(同 W32-A):操作控制 CAS 预期在手填模式下由 operator 提供;接线后可由进程查询直接读取 generation。
4. **deferred minors**:`parity_check` 读路径 operation 下拉未加 resource 域读命令(仅写路径探针扩展;补齐属 W32-C 矩阵);kill 两步确认窗口固定 5 秒、无偏好设置;前端 reason 无长度上限提示(上游 wire 前仅要求非空,未擅自加约)。

### W32-B.4 工件清单(本波次写集)

- `desktop/src-tauri/src/{ipc,dto,lib,devfixture}.rs`、`desktop/src-tauri/tests/authenticated_write_side.rs`(新增)、`tests/authenticated_read_side.rs`(import 更名)、`desktop/src/{main,ipc,types}.ts`、`desktop/src/style.css`、`desktop/README.md`。
- 本证据文件 §W32-B(唯一 desktop/ 外写集工件)。

## §W32-C GUI↔NL↔CLI Receipt parity 钉死(2026-09-21)

### W32-C.1 实现范围

gate(B5-6):GUI↔NL↔CLI 三路径 Receipt 逐字节相等测试入库。

1. **仓库级 parity 套件**:`crates/nlos-system-control/tests/triple_path_receipt_parity.rs`(新增,随 `cargo test --workspace` 进 CI)。对 SABI v1.4 全部 20 个 `ControlCommand` 变体按家族分组,一个双入口夹具(ADR-0011 认证入口 + plain 入口服务同一个 `SqliteTaskAuthority`——artifact escalated 计划 + semantic/resource escalated ledger 行、同一 `StubHealth`、同一 `CapabilityPolicy`、同一 `DeterministicOperationExecutor`)同时服务四路派发,任何字节差只能来自派发路径本身:
   - **direct**:`dispatch_in_process` 参考投影;
   - **NL**:`parse_nl_command` 句子编译(先断言编译结果 == 直接构造的同一命令)再经 plain 入口派发;
   - **CLI**:真实 `system-control-cli` 二进制子进程(`CARGO_BIN_EXE_*`,plain 入口,stdout 首行 `RECEIPT <hex>`);
   - **GUI**:GUI 后端命令层的 dispatch 核心经认证入口派发(见下)。
   三入口时间常量贯穿一致(`FixedWall(42_000)` + monotonic 10),回执时间戳字节可比。
2. **GUI 腿的诚实机制(选定并文档化)**:desktop 后端命令层 `desktop/src-tauri/src/ipc.rs::dispatch_control` 是 `nlos_system_control::auth::dispatch_over_authenticated_socket` 的薄类型化外壳(principal/密钥文件解析 + Ed25519 签名闭包 + DTO 投影,`receipt_hex` 恒为 `receipt_to_hex` 输出)。仓库级测试无法依赖 desktop crate(独立 workspace、tauri 依赖树、`npm run build` 前置),因此 GUI 腿直接驱动**同一 dispatch 核心**经 ADR-0011 认证入口——GUI 的唯一接线方式,与 W32-A README 对 W32-C 的设计预告一致。外壳函数本身的字节透明性另由 desktop 侧 W32-A/B 集成测试活体钉住(真实调用 `dispatch_control`,认证 vs plain 回执字节一致;`build_control_command` 编译点单测)。**desktop 后端零改动**——headless 钩子已存在(pub `dispatch_control`/`build_control_command`),无需新增测试钩子。
3. **三形态矩阵**(对齐 `control_ipc_auth.rs` 既有 parity 测试,扩展到全家族 + CLI 子进程腿):成功读;typed `NotFound`(missing plan 的 InspectTask、未接线 inspector 的 InspectProcess/InspectResource——CLI 与 GUI 生产形态同为客户端 inspector `None`);typed `Rights`(denied 前缀 reason 的 ack 与 pause;NL 语法固定审计 reason 无法表达拒绝,该腿仅在 direct/CLI/GUI 三路,已注明)。mutation 家族:ack 幂等重放同字节(四路连续派发同一命令);resume 类每腿派发前重置 ledger 行回到 `Escalated`(与 `control_command_cli.rs` 同法),并在断言中确认真实 CAS mutation 生效(acknowledged / Retrying)。
4. **NL 语法边界钉死**:SABI v1.4 NL 语法无 semantic 域形式——4 个代表性 semantic 句子(`inspect semantic health`/`export semantic metrics`/`acknowledge semantic alert …`/`resume semantic recovery …`)断言为 typed `InvalidCommand`(派发前拒绝);semantic 家族跑 direct/CLI/GUI 三腿字节一致。补齐 semantic NL 语法属后续波次(本波写集不含 `nl.rs`)。
5. **平台纪律**:整文件 `#![cfg(all(unix, feature = "cli"))]`,Windows 腿零编译(本波两次 Windows clippy 事故的预防)。夹具 socket 路径命名压缩(`nlos-sc-3p-*`):per-user TMPDIR(`/var/folders/…/T/`)下路径逼近 macOS `SUN_LEN`(104 字节)上限,首版长命名在最长 label 上间歇性触发 socket2 `SUN_LEN` 拒绝——已定位并以短前缀 + 连续 8 轮重复运行零 flake 复验。

### W32-C.2 覆盖矩阵(命令家族 × 路径)

| 家族 | 命令 | direct | NL | CLI | GUI | 回执形态 |
|---|---|---|---|---|---|---|
| 聚合巡检 | InspectHealth / InspectSemanticHealth / InspectResourceHealth | ✓ | ✓ / –(typed reject 钉) / ✓ | ✓ | ✓ | 成功 |
| 指标导出 | ExportMetrics / ExportSemanticMetrics / ExportResourceMetrics | ✓ | ✓ / –(typed reject 钉) / ✓ | ✓ | ✓ | 成功 |
| 范围巡检 | InspectTask(escalated 计划) | ✓ | ✓ | ✓ | ✓ | 成功 |
| 范围巡检 | InspectTask(missing plan) | ✓ | ✓ | ✓ | ✓ | typed NotFound |
| 范围巡检 | InspectProcess / InspectResource(inspector 未接线) | ✓ | ✓ | ✓ | ✓ | typed NotFound |
| artifact 恢复 | AcknowledgeRecoveryAlert | ✓ | ✓(命令 id 派生自 plan id) | ✓ | ✓ | 成功,幂等重放同字节 |
| semantic 恢复 | AcknowledgeSemanticRecoveryAlert / ResumeSemanticRecovery | ✓ | –(typed reject 钉) | ✓ | ✓ | 成功,resume 每腿重臂 |
| resource 恢复 | AcknowledgeResourceRecoveryAlert / ResumeResourceRecovery | ✓ | ✓(命令 id 派生自 plan id) | ✓ | ✓ | 成功,resume 每腿重臂 |
| 操作控制 | Pause / Resume / Cancel / Kill / Throttle / Reclaim Operation | ✓ | ✓(命令 id 派生自 target id) | ✓ | ✓ | 成功(确定性执行器缝) |
| 拒绝形态 | AcknowledgeRecoveryAlert / PauseOperation(denied reason) | ✓ | –(NL 固定 reason) | ✓ | ✓ | typed Rights |

测试名(`triple_path_receipt_parity`,6 项):

1. `inspect_export_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths`
2. `scoped_inspect_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths`
3. `artifact_recovery_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths`
4. `semantic_recovery_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths`
5. `resource_recovery_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths`
6. `operation_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths`

### W32-C.3 验证(本机实跑,macOS/darwin arm64)

- `cargo test -p nlos-system-control`:**全绿**(既有 12 个测试目标无回归;新套件 6 项全过)。新套件连续 8+ 轮重复运行零 flake(SUN_LEN 路径竞态修复后)。
- `cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`、`cargo clippy --all-targets -- -D warnings`:0 warning。
- desktop(未改任何 desktop 代码,确认外壳活体证据仍绿):`npm run build` 通过;`desktop/src-tauri` `cargo test --features dev-fixture` 8 项全过(4 读 + 4 写;1 项 ignore 为 CLI 二进制探针)。
- 未运行/未验证:Windows 实机(整文件 cfg 空,零 Windows 腿代码);`--no-default-features` clippy 在基线即失败(`control_command_cli.rs` 既有状态,非本波引入,CI 使用默认 feature 集)。

### W32-C.4 边界与遗留

1. **NL semantic 语法空缺**:semantic 域命令无 NL 形式(已钉为 typed 拒绝);补语法是后续波次的 `nl.rs` 写集。
2. **GUI 腿层级**:仓库级测试驱动 dispatch 核心而非 Tauri 命令包装(`tauri::State` 无法 headless 构造);外壳层字节透明由 desktop 侧集成测试活体覆盖。desktop Rust 测试不在根 workspace CI(`cargo test --workspace` 不含独立 workspace 的 `desktop/src-tauri`;手动步骤在 desktop README)。
3. **deferred minors**:NL 同义词覆盖在 W32-C 取样(canonical EN + ZH 各一),全量同义词表已由 `control_command_cli.rs` 钉死;`parity_check` 读路径下拉的 resource 域读命令补齐(W32-B 遗留)仍未做。

### W32-C.5 工件清单(本波次写集)

- `crates/nlos-system-control/tests/triple_path_receipt_parity.rs`(新增,唯一代码写集)。
- 本证据文件 §W32-C 与头部状态行、`docs/management/evidence-index.yaml` b-gui-001 行更新。

## §W32-D 权限/预算可见半(2026-09-21)

### W32-D.1 实现范围

gate:「权限变更 UI 可见且与 authority 一致」的最小诚实版(B5-5 前半:授权/预算/成本可见)。纪律:只渲染 inspect 面真实暴露的事实;无 IPC 面的权威事实登记缺口,不发明数据。

1. **「权限/预算」视图(前端四卡)**:
   - **控制面授权事实**:会话 principal(ADR-0011 认证身份)、resource_root 接线状态、SystemControl 服务名与固定控制能力句柄(`CONTROL_CAPABILITY_SLOT=9`/`GENERATION=1`)——均为客户端路径事实(每条派发信封携带,`request_context` 组装;服务端授权检查拒绝时回执为类型化 `RIGHTS` 失败),显式标注「非 inspect 数据」;「验证控制面授权」按钮经真实 dispatch(`inspect-resource-health`)取回执佐证。
   - **预算/成本查询**:`inspect_resource_cost` 经认证入口派发 `InspectResource`;会话配置 `resource_root`(env `LLMOS_DESKTOP_RESOURCE_ROOT` 或连接配置页,不落盘)时由后端以真实 `ResourceAuthorityInspector`(每次派发即时 `ResourceAuthority::open`,WAL 多进程读安全,不缓存句柄)组装五个有界事实(reservation_id/account_id/upper_bound/usage_high_water/consumption_count,投影自权威 `inspect_cost_receipt`——只对已结清 FINALIZED 预留开放的不可变聚合)+ 一行明确标注「派生」的结余(upper_bound − usage_high_water,确定性算术,非新事实);未配置时 inspector 传 `None`,回执为诚实的类型化 `NOT_FOUND`(与 CLI 字节一致),绝不伪造预算数据。
   - **成本事实自检**:`cost_fact_check` 把同一 reservation 两次独立经认证入口派发,逐字段比较渲染事实与直接复检 + receipt hex 比对(结清事实不可变 → 必须 matched;未接线形态两侧同 `failure/NOT_FOUND` 亦如实可比)。
   - **缺口登记卡(静态)**:列出无 IPC inspect 面的权威事实(见 W32-D.3),声明「本视图不渲染」。
2. **后端命令面**:`dispatch_control_with_resource`(认证 dispatch 核心的可选 inspector 扩展;`dispatch_control` 保持 None/None 委托,既有命令与 parity 路径字节不变)+ `inspect_resource_cost`/`cost_fact_check`/`control_plane_facts` 三命令;`SessionConfig` 增 `resource_root`;`Cargo.toml` 增 `nlos-resource` path dep 并对 `nlos-system-control` 启用 `resource` feature(上游既有 `ResourceAuthorityInspector` 适配器,零 crates/ 改动)。
3. **一致性纪律(parity 模式不受影响)**:CLI 字节比对路径(`parity_check`/`dispatch_control`)恒未接线——有专门集成测试钉死(`parity_dispatch_path_never_wires_the_resource_inspector`);接线后的成本回执与 CLI(未接线)字节不同属预期,该路径是「本地权威直读视图」而非 CLI parity 面;「与 authority 一致」由 `resource_cost_inspect_matches_authority_facts` 对权威 `inspect_cost_receipt` 直接读数逐字段钉死(含消费回执表行数 == consumption_count)。
4. **开发夹具扩展**:devfixture 增真实 `ResourceAuthority` 全链(driver→account→quote→reserve→activate→consume×2→finalize),产出已结清预留(上界 100/高水位 70/2 次消费);`dev_server` 打印 `LLMOS_DESKTOP_RESOURCE_ROOT` 与 reservation_id/account_id。
5. **顺带清偿 W32-B deferred minor**:`parity_check` operation 下拉补 `inspect-resource-health`/`export-resource-metrics`(两条快照类读命令,GUI 未接线 vs CLI 未接线字节一致)。

### W32-D.2 验证(本机实跑,macOS/darwin arm64)

- `desktop/`:`npm install` → `npm run build`(tsc 严格 + vite 7)通过。
- `desktop/src-tauri/`:`cargo fmt --check` 通过;`cargo clippy --all-targets --features dev-fixture -- -D warnings` 与默认 feature 两态 0 warning;`cargo build` 通过。
- `cargo test --features dev-fixture`:**19 项全过**(7 单元 + 4 读侧 + 4 写侧 + 4 权限侧新增):
  - `resource_cost_inspect_matches_authority_facts`:经认证入口 + 真实 inspector 的成本回执五字段 == 夹具确定性事实,且 == 权威 `inspect_cost_receipt` 直接读数(upper_bound/finalization.high_water/consumptions.len())——「与 authority 一致」活体证据;
  - `cost_fact_check_matches_across_two_dispatches`:两次独立派发 fact_check 全行 matched + receipt hex 相同;
  - `unwired_cost_inspect_stays_typed_not_found_and_matches_plain_entry`:未接线形态类型化 `NOT_FOUND`(「not wired」),且与 plain 入口(CLI 同路)receipt 字节一致;未接线形态下 fact_check 亦 matched;
  - `parity_dispatch_path_never_wires_the_resource_inspector`:`dispatch_control`(CLI parity 路径)在配置了权威的环境下仍回未接线 `NOT_FOUND`(接线不泄漏进 parity 面);
  - 单元:`parity_command_covers_resource_domain_reads`(下拉新增两命令的编译点)。
- 真实 CLI 活体探针:`cargo build -p nlos-system-control` 后 `LLMOS_SYSTEM_CONTROL_CLI=… cargo test --test authenticated_read_side -- --ignored` 通过(真实二进制 plain 入口 receipt hex 与 GUI 认证入口逐字节相等)。
- `dev_server` 实跑:打印 `LLMOS_DESKTOP_RESOURCE_ROOT=<临时目录>/resource`、`reservation_id`/`account_id` 与确定性事实(upper_bound=100 usage_high_water=70 consumption_count=2)。
- 未运行/未验证:GUI 窗口内交互式点按(同 W32-A/B 限制,无 Accessibility/Screen Recording;以命令层集成测试 + 夹具数据替代,README 演示步骤供人工复验);宿主生产资源权威的自动发现(resource_root 由 operator 显式提供)。

### W32-D.3 缺口登记:权威事实存在、IPC inspect 面不存在(不渲染,待后续车道)

以下事实已在本地权威持久化(crate 有真实数据与读数 API),但 `ControlCommand` 无对应 arm、`system_control.proto` 无对应视图——本波次不发明数据,UI 以静态缺口卡声明缺席:

| # | 权威事实 | 权威读数 API(crates 内) | 缺的 IPC 面 |
| --- | --- | --- | --- |
| 1 | 能力签发/衰减/撤销账本(CapabilityRecord:issuer/holder/rights/target/有效期/衰减深度/parent 链) | `nlos-capability::CapabilityAuthority::inspect_active` | 无 ControlCommand arm,proto 无 capability 视图 |
| 2 | 能力调用限额剩余与消耗回执 | `CapabilityAuthority::call_limit_remaining` / `capability_consumption_rows` | 同上 |
| 3 | 资源报价明细(demand_capacity 三维/pricing_version/valid_until/报价上界) | `nlos-resource::ResourceAuthority::inspect_quote` | 只有 upper_bound 进入 `ResourceInspection` 有界投影;报价本身无视图 |
| 4 | 预留状态机(Reserved/Active/Quarantined/Finalized)与多维需求(cpu_shares/memory_mib/io_weight) | `inspect_permit_binding` / `ReservationRecord.demand` | 不在五个有界字段内 |
| 5 | 账户预算余额(initial/available credit) | `resource_accounts` 表(经 create/reserve 间接可推) | 无 inspect 面 |
| 6 | 结清明细回执(FinalizationReceipt.refund_credit、逐条 ConsumptionReceipt) | `inspect_cost_receipt` 聚合内 | 只有高水位与条数进入投影;明细无 IPC 面 |

补面属后续车道(需同时扩 proto 视图与 ControlCommand,超出本车道写集);本登记同时复制到 UI 缺口卡(`desktop/src/main.ts::IPC_SURFACE_GAPS`)。

### W32-D.4 边界与遗留

1. **生产接线形态**:宿主 NLOS 的资源权威根目录由 operator 以 `resource_root` 显式提供(夹具演示全自动);后续宿主接线波次可改为权威广播/约定路径发现。
2. **`RIGHTS` 拒绝形态**:渲染逻辑就绪(授权验证卡按回执 `RIGHTS` 码判红),开发夹具授权策略固定通过固定句柄,不产生该形态;活体证据待 W32-C Rights 形态矩阵。
3. **`tauri build` 完整打包**:同前波次边界(`bundle.active=false`)。
4. `docs/management/stage-b-progress.md` 波次表更新不在本车道写集(integrator 收口)。

### W32-D.5 工件清单(本波次写集)

- `desktop/src-tauri/src/{ipc,dto,lib,devfixture}.rs`、`desktop/src-tauri/Cargo.toml`、`desktop/src-tauri/examples/dev_server.rs`、`desktop/src-tauri/tests/authenticated_permission_side.rs`(新增)、`desktop/src/{main,ipc,types}.ts`、`desktop/README.md`。
- 本证据文件 §W32-D(唯一 desktop/ 外写集工件)。

## §W32-E 资源监控半(2026-09-21)

### W32-E.1 实现范围

gate(B5-5 后半):「消费既有 metrics 面,无新控制路径」——Resource Monitor 最小版 = OpenMetrics 消费 + 展示。

1. **消费机制(结论:OpenMetrics 文本经 SABI IPC 真实可达,无需回退)**:`ControlCommand::ExportMetrics`/`ExportSemanticMetrics`/`ExportResourceMetrics`(W27-A semantic 目录 + G8 resource 目录 + artifact 基线目录的三条既有只读命令)的回执 outcome 即 `ControlOutcome::MetricsExported { openmetrics_text }`——运行时 `OpenMetricsRenderer::render` 的确定性 `text/plain; version=0.0.4` 文本。桌面后端因此**不走** in-process/self-check 回退:每次取数就是一条经 ADR-0011 认证入口的真实 dispatch,与 CLI 同一回执面(集成测试钉死字节一致)。后端唯一增量是 `export_resource_metrics` 薄命令——resource 域导出的 GUI 接线(W32-A 只接了 artifact/semantic 两域),消费既有命令面,非新控制路径。
2. **「资源监控」视图**(`desktop/src/main.ts`):手动「刷新(三域,经认证 IPC)」+ 可选 5 秒自动刷新(每次刷新 = 三条完整真实认证 dispatch,拉模型)。三域各一张卡:结构化指标表(指标族/类型/标签/值),由 `desktop/src/openmetrics.ts` 对确定性文本做严格解析——未识别的行原样显示、不静默丢弃;前缀不属于本域的族单独分组如实呈现;计数器值是 u64 十进制文本,按原样显示不做 JS number 换算(u64 超出安全整数会失真)。原始 OpenMetrics 文本折叠在 `<details>` 内可展开比对;卡片页脚恒显 `control_command_id`/`correlation_id`/`receipt_hex`(与 CLI `RECEIPT` 行同一等价契约)。类型化失败(非 `metrics_exported` outcome)按既有 SabiFailure 卡渲染,不伪造零值。
3. **无新控制路径**:视图纯展示,唯一派发面是三条只读导出命令;W32-B 动作面零改动(`submit_control` 不被本视图触碰);视图内静态「指标面缺口登记」卡声明消费边界。
4. **顺带清偿(写集内必要债,W32-B §5 同款先例)**:W32-G(SABI v1.5,已在 HEAD 合入)给 `ControlOutcome` 增加了五个四层 inspect 变体(TaskGroup/TaskNode/ExecutionFiber/Topic/DurableOperation Inspected),desktop 是独立 workspace 不进根 CI,`dto.rs` 的穷尽匹配在基线即编译失败(E0004,`cargo check` 可复现)——本波次以显式 DTO 形态补齐(有界标量投影;TaskGroup 成员行以计数 + 截断标志呈现,不逐行展开;TaskNode 的 `PlanNodeKind` 字段命名 `node_kind` 以避让 serde tag `kind`;`types.ts` 同步镜像)。五个视图的 GUI 命令接线属后续车道。

### W32-E.2 验证(本机实跑,macOS/darwin arm64)

- `desktop/`:`npm install` → `npm run build`(tsc 严格 + vite 7)通过。
- `desktop/src-tauri/`:`cargo fmt --check` 通过;`cargo clippy --all-targets --features dev-fixture -- -D warnings` 与默认 feature 两态 0 warning;`cargo build` 通过(基线 E0004 已由 §W32-E.1 第 4 条清偿)。
- `cargo test --features dev-fixture`:**21 项全过**(7 单元 + 4 读侧 + 4 写侧 + 4 权限侧 + 2 监控侧新增):
  - `metrics_exports_carry_three_recovery_domains_over_authenticated_entry`:三条导出命令经认证入口回执均 `metrics_exported`,三域目录逐族在场且取值 == 夹具权威健康事实(artifact:worker_state `backing_off`=1、cycles_total=4、plans_inspected=3、plans_finalized=2、durable_escalated=1;semantic/resource 目录在场、夹具上全 0)——消费既有面,不发明指标;
  - `metrics_export_receipts_match_plain_entry_bytes`:同批三条命令经 plain 入口(CLI 同路)派发,认证 vs plain receipt hex 逐字节相等(三域导出纳入 GUI↔CLI parity 面)。
- 真实 CLI 活体冒烟:`cargo build -p nlos-system-control` 后 `dev_server` 启动,真实 `system-control-cli <plain_socket> export-metrics | export-semantic-metrics | export-resource-metrics` 三命令均回 `outcome=metrics_exported`(文本 1363/832/832 字节);receipt hex 解码确认 worker 生命周期状态机样本(`backing_off` 1、其余 0)与目录族名/取值。
- 未运行/未验证:GUI 窗口内交互式点按(同前波次限制,无 Accessibility/Screen Recording 权限;以命令层集成测试 + 夹具 CLI 冒烟替代,README「资源监控」演示步骤供人工复验)。

### W32-E.3 指标面缺口登记(消费边界;视图内静态卡同步)

1. **scrape/流式指标端点不存在**(B-TASK-006M 未竟项:HTTP scrape/ETW/订阅端点、retention/alert rules)——「自动刷新」是每 5 秒三条只读导出命令的重新真实派发(拉模型),不是推送流。
2. **宿主级资源用量指标无既有导出面**:进程 CPU/内存/IO、预留实时用量等不在恢复指标目录(三域 26 族 + worker 生命周期)内,任何既有面都不导出——视图不发明(本最小版「资源监控」语义 = 三恢复域资源指标监控;宿主用量目录待后续指标车道扩目录)。
3. **resource 域 GUI 导出接线缺口(W32-A 遗留,本波次关闭留档)**:W32-A 只接了 artifact/semantic 两域导出命令;W32-E 以既有 `ControlCommand::ExportResourceMetrics` 补上(只读;SABI 面本身无缺口)。

### W32-E.4 边界与遗留

1. **任务预案第 3 条不触发**:「metrics-over-IPC 缺面则回退 in-process/self-check 模式」——实测 OpenMetrics 文本经 IPC 回执可达,消费全走认证入口,无回退路径、无第二套指标源。
2. **deferred minors**:自动刷新固定 5 秒、无偏好设置;视图切走不打断刷新计时器;前端解析器无独立测试基建(仓库 desktop 无 TS 测试运行器,解析纪律由 Rust 集成测试对同一确定性文本钉死 + 未识别行如实显示兜底)。
3. **五层 inspect(SABI v1.5)GUI 接线**未做(`dto.rs`/`types.ts` 形态已补齐穷尽;命令与视图属后续车道)。
4. `docs/management/stage-b-progress.md` 波次表更新不在本车道写集(integrator 收口)。

### W32-E.5 工件清单(本波次写集)

- `desktop/src-tauri/src/{ipc,dto,lib}.rs`、`desktop/src-tauri/tests/resource_monitor_metrics_side.rs`(新增)、`desktop/src/{main,ipc,types,openmetrics}.ts`(`openmetrics.ts` 新增)、`desktop/README.md`。
- 本证据文件 §W32-E 与头部状态行、`docs/management/evidence-index.yaml` b-gui-001 行更新。

## §W32-F 应用表面呈现(2026-09-21)

> 车道:W32-F / B2-2——ROAD-B-002 四能力维度中「UI Surface」的声明→呈现最小链(W33-H review §4 行 6 判定的唯一硬阻塞项)。按本文件 W32-x 节先例落档于 b-gui-001(横跨 nlos-application manifest 半 + desktop 呈现半)。

### W32-F.1 实现范围

gate 措辞:「Application 声明 surface→窗口呈现最小链」。判定口径:声明 → 呈现,不是窗口管理系统(无生命周期管理/焦点路由/几何/多窗口编排)。

1. **manifest additive `surfaces` 段(`crates/nlos-application`,W28-B tasks 段先例)**:
   - 声明模型 `PackageSurfaceDeclaration { surface_id(16B 段内唯一), kind(Window|Panel), title, entry_name?(manifest entry 名的内容引用) }` + 共享 typed 校验 `validate_surface_declarations`(非空、准入界 100K、唯一 id、title/entry 文本界 1..=255B 无 NUL——`PackageTaskTemplate` 同款纪律);
   - **durable 登记面(schema v8 `application_surface_registrations`)**:`register_surfaces` 把一个声明段按幂等键登记到应用**当前安装代际**,并做**内容绑定**——请求声明的 manifest digest 必须等于应用行当前安装的 manifest digest(声明陈旧内容 → 类型化 `SurfaceManifestMismatch` 拒绝,绝不静默改绑);DDL 三 trigger(不可变/不可删/state-bounds:installed 状态 + 当前代际 + digest 相等)与 v5/v6 登记面同构。表面身份在一代内唯一(`UNIQUE(application_id, surface_id, generation)`),代际推进(update)后重开准入;
   - **inspect 面**:`inspect_surfaces(package_id)` 按登记序+声明序逐位读回声明内容——呈现侧的发现面,durable 行即应用声明的事实;
   - 签名包文件格式(`nlos/package-file/v1`)在 nlos-artifact(本车道写集外),故声明段以应用侧公共 API 登记并经 manifest digest 绑定到已验签安装的内容(install 的 digest-binding 纪律的最小镜像),不发明第二签名面。
2. **desktop 呈现(`desktop/src-tauri/src/surfaces.rs` + `ipc.rs` present_surfaces 命令 + `main.ts`「应用表面」视图)**:
   - 机制 = W32-D resource_root 同款本地权威直读:会话配置 `application_root`(env `LLMOS_DESKTOP_APPLICATION_ROOT` 或连接配置页)→ 每次呈现即时 `ApplicationAuthority::open`(WAL 多进程读安全,不缓存句柄)→ `inspect_application` + `inspect_surfaces` → DTO 投影。本视图**没有未接线回退形态**:未配置 → 类型化 `CONFIG` 拒绝;
   - **stale 代际不呈现**(DUI-WINDOW-001 最小版):只呈现登记在当前安装代际的表面;update 推进代际后旧登记仍是 durable 事实但不进可呈现集,gen 2 重声明后恢复;非 installed 状态如实投影空集 + 状态行;从未安装的包 → 新增类型化错误码 `NOT_FOUND`(事实读回,非重试面);
   - **窗口形呈现**:每个可呈现表面渲染为窗口/面板卡(标题栏 kind 徽标 + 声明 title;元数据行 surface_id/kind/title/entry_name/登记幂等键/时间;明示内容占位)。DTO 零发明字段——呈现的每一行是 durable 声明事实的逐位投影;entry 载荷渲染属后续车道(缺口卡登记)。
3. **端到端最小链测试**(`desktop/src-tauri/tests/surface_presentation_side.rs`,W32-A parity-mode 风格——对命令层呈现数据无头断言):小夹具样例包(单 entry + surfaces 声明段)→ 真实签名验签(ArtifactStore + IdentityAuthority 权威管线)→ `install_application`(verify-then-commit)→ `register_surfaces`(代际+digest 绑定)→ **第二个独立打开的权威句柄上的桌面呈现**(读者进程形态)→ 逐字段断言(application_id/generation/status/manifest_digest hex、两表面声明序、kind/title/entry、登记键 hex)。

### W32-F.2 验证(本机实跑,macOS/darwin arm64,rustc 1.97.1,Node 26.3.0)

- `crates/nlos-application`:`cargo fmt --check` 通过;`cargo clippy --all-targets --all-features -- -D warnings` 0 warning;`cargo test -p nlos-application` **85 passed / 0 failed**(单元 12[含 surfaces 校验/编码 3 项] + application_authority 43 + 故障注入 7 + migration 11 + manifest_task_templates 4 + **surface_registration 8 新增**)。既有 goldens(含 W28-B g6 逐字节面)零改动全绿——v8 是纯增量迁移,additivity golden 成立(surface_registration 首测同时断言登记不动应用行任何字段)。
- `desktop/`:`npm install` → `npm run build`(tsc 严格 + vite 7)通过;`desktop/src-tauri/`:`cargo fmt --check` 通过;`cargo clippy --all-targets --features dev-fixture -- -D warnings` 与默认 feature 两态 0 warning;`cargo test --features dev-fixture` **25 项全过**(单元 9[含 surfaces 单元 2 项] + 4 读侧 + 4 写侧 + 4 权限侧 + 2 监控侧 + **2 表面链新增**):
  - `declared_surfaces_present_through_the_desktop_command_face`:验签→install→登记→第二句柄呈现,全字段 == 安装/登记事实;未配置 application_root 类型化 `CONFIG`;
  - `stale_generation_and_unknown_package_present_honestly`:update 代际 1→2 后旧登记不呈现(durable 行仍在)、gen 2 重声明恢复呈现(title v2)、未知包类型化 `NOT_FOUND`。
- `python3 scripts/lint_claims.py`:PASS(143/143,索引含本节所在文件既有行,无新增文件)。
- 未运行/未验证:GUI 窗口内交互式点按(同前波次限制;以命令层集成测试替代,README「应用表面」演示步骤供人工复验);`--workspace` 全仓门/三平台 CI(波次屏障统一跑);nlos-application 非 Unix 契约道未本机实测(SQLite+std,平台无关代码)。

### W32-F.3 呈现边界登记(诚实边界;视图内静态缺口卡同步)

| # | 缺口 | 说明 |
| --- | --- | --- |
| 1 | entry 载荷内容渲染 | `entry_name` 引用的 manifest entry 载荷字节(artifact store 物化内容)未进入呈现——视图渲染声明元数据 + 内容占位,不伪造内容 |
| 2 | 表面生命周期管理 | REGISTERED→CREATED→PRESENTED↔HIDDEN→CLOSED 状态机、open/close 动作、stale 隔离执行器——本链只有 durable 声明与呈现过滤 |
| 3 | 焦点/输入路由与几何 | DUI-WINDOW-001/DUI-INPUT-001 的 focus/input route、accessibility tree、窗口几何/多窗口编排——未建模 |
| 4 | Surface 域 SABI IPC 面 | 呈现走本地权威直读(application_root 接线,与 W32-D 成本查询同机制);Surface 域 ControlCommand(register/create/present/…)不在本波次 |

### W32-F.4 边界与遗留

1. **登记入口是库 API**:`register_surfaces` 由应用侧(样板驱动形态)调用;`sample-app-driver` 子命令与 `nlos package` CLI 段解析属后续车道(nlos-artifact 写集外,packaging.md 的 manifest 行式扩展随之)。
2. **单读者直读形态**:application_root 由 operator 显式提供;宿主权威广播/约定路径发现、呈现经 SystemControl IPC 的面属后续接线。
3. deferred minors:登记不进 W27-D 活动门解析源(表面非活动物,无 teardown 语义——与 background task 面有意不同);`tauri build` 完整打包同前波次边界。
4. `docs/management/stage-b-progress.md` 波次表/W33-H §5 行更新建议的采纳不在本车道写集(controller/integrator 收口)。

### W32-F.5 工件清单(本波次写集)

- `crates/nlos-application/src/{surfaces.rs(新增),lib.rs,schema.rs}`、`crates/nlos-application/tests/surface_registration.rs`(新增)。
- `desktop/src-tauri/src/{surfaces.rs(新增),ipc.rs,dto.rs,error.rs,lib.rs}`、`desktop/src-tauri/Cargo.toml`(+nlos-application path dep;dev-fixture 增 nlos-artifact 可选依赖)、`desktop/src-tauri/tests/surface_presentation_side.rs`(新增)、`desktop/src/{main,ipc,types}.ts`、`desktop/src/style.css`、`desktop/README.md`。
- 本证据文件 §W32-F 与头部状态行、`docs/management/evidence-index.yaml` b-gui-001 行更新。
