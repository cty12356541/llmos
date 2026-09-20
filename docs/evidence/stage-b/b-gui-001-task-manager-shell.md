# B-GUI-001:可信 Tauri 任务管理器壳(W32-A 只读半 + W32-B 写入半)

> 状态:`PARTIAL PASS`(读半 W32-A + 写入半 W32-B 已落地;parity 钉死 W32-C 未做)
>
> 日期:2026-09-20(W32-A)/ 2026-09-21(§W32-B)
>
> 对应:`ROAD-B-005`/B5-4 读半边、`[SABI-AUTH-001]`、`[CTRL-PARITY-001]`、[ADR-0011](../../management/adrs/0011-ipc-principal-auth-signature-passthrough.md)、[B-TASK-006L](./b-task-006l-system-control-recovery-handler.md)、[B-SCHEMA-006](./b-schema-006-typescript-python-ipc-clients.md)

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
