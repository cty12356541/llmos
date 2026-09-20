# B-GUI-001:可信 Tauri 任务管理器壳(W32-A 只读半)

> 状态:`PARTIAL PASS`(只读半;写入半 W32-B、parity 钉死 W32-C)
>
> 日期:2026-09-20
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
