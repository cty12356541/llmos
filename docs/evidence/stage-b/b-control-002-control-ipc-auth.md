# B-CONTROL-002：SystemControl IPC 面接入 ADR-0011 认证（opt-in）

> 状态：`PASS`（单节点本地）　　日期：2026-08-30
>
> 对应：`B-TASK-006L`、[ADR-0011](../../management/adrs/0011-ipc-principal-auth-signature-passthrough.md)（决定 1 连接级 challenge-response、决定 3 AuthorityClock）、总纲 §25.3、[B-CONTROL-001](b-control-001-control-command-cli.md) 已知限制 3 的闭环
>
> 前置：W2-A 设施（`nlos-ipc::handshake::transport`，HEAD `2c424da`）+ W2-E 成果（`nlos-clock` wall 读数 / `nlos-identity` at-clock 验签，HEAD `4c1387f`）

## 已实现事实

1. **additive 边界**：`nlos-system-control` 新增 `auth` 模块（`#[cfg(all(unix, feature = "cli"))]`，`cli` 为既有默认 feature）。既有 in-process 路径、`dispatch_over_socket` 本地信任域路径与全部 conformance 依赖的行为零改动——`ControlCommand` 编译、`RecoverySystemControl::handle_for_ipc`、`ControlReceipt::compose` 单点投影全部原样复用；本模块只新增一个显式 opt-in 服务入口和一个认证 dispatch 客户端，不存在第二条控制语义路径。
2. **认证服务入口** `authenticated_serve_one_control(listener, config, control, identity, clock, handshake, peer_gate, now_monotonic_ns, next_nonce)`：消费 W2-A 的 `nlos_ipc::handshake::transport::authenticated_serve_one` + `ServerHandshakeContext`。握手 verified_at 取 **`AuthorityClock` 的 durable wall 读数**（ADR-0011 决定 3）：以一次性 server nonce 经域分隔 SHA-256（`llmos/control-auth/handshake-wall/v1`）派生 16 字节幂等键调 `wall_now`，故每次连接观察新鲜、单调的读数，重放握手永远无法重读过期时刻；时钟拒绝服务时 fail-closed（`HandshakeError::Identity(IdentityAuthorityError::Clock(..))`），不猜测时间。任何握手失败（含 pre-gate 拒绝、坏签名、重放 nonce、未知 principal、binding 漂移、key 过期/撤销）都在请求字节派发前拒绝连接，类型化 `HandshakeError` 原样透传。
3. **命令级时间语义（选定：请求关联）**：served exchange 的 `now_wall_ms` 是 `AuthorityClock` 以**请求 §25.3 correlation id 直接作幂等键**（`command_wall_key`，pub，已文档化）签发（或 durable replay）的 wall 读数。mutation 的 correlation id 与命令 idempotency key 由既有 handler 的 `CommandIdempotencyMismatch` 守卫绑定，因此同一命令重试重读原始 durable 读数、回执时间戳跨重试稳定（与 `verify_capability_command_signature_at_clock` 的 replay 语义互为镜像）。correlation 非 16 字节（`REQUEST_ID_BYTES`）时返回类型化 `INVALID_ARGUMENT` 失败 envelope，绝不猜测时间——新增 `SystemControlError::UnboundedCorrelation` / `ClockWallUnavailable` 两个 additive 变体（仅认证路径产生，Display/source/`to_sabi_failure` 同步映射，经 `failure_envelope` 单点脱敏投影），既有本地信任域路径永不产生。
4. **认证客户端** `dispatch_over_authenticated_socket(socket, principal, sign, command)`：消费 `authenticated_connect` 回答挑战后，走与 `dispatch_over_socket` 完全相同的 `LocalRpcClient` 传输、handler 与回执投影；握手拒绝映射为新增 additive `ControlError::Handshake` 变体（Unix + `cli` only，与既有 `ControlError::Ipc` 的 cfg 模式一致）。
5. **依赖面（opt-in）**：`cli` feature 追加 optional 依赖 `nlos-clock`、`nlos-identity`、`sha2`（握手 verified_at 派生）+ dev-dep `ed25519-dalek`（测试真签名）；`--no-default-features`（含 Windows 非 cli 形态）不编译 `auth` 模块，不引入这些依赖。
6. **测试** `tests/control_ipc_auth.rs`（unix-only，8 用例）：认证 roundtrip（真 Unix socket + 真 `IdentityAuthority` Ed25519 严格验签 + 真 durable `AuthorityClock`（SQLite WAL/FULL，注入固定 wall source 使判定时刻可断言）下 inspect + acknowledge 全链路成功，verified principal/key id/generation 断言，`wall_now(correlation key)` 断言为 `Replayed(42_000)` 且跨新 clock 句柄（重启形态）durable replay 不变，durable alert 恰被确认一次）；负路径坏签名（`SignatureInvalid`、连接关闭、nonce 被烧毁）、重放 attestation（第二连接 `NonceRejected`）、未知 principal（`PrincipalUnknown`）、binding 不匹配（`ChannelBindingMismatch` 且 nonce 未被烧毁）、**key 有效期按 clock wall 读数判定**（binding `valid_until=10_000` < clock `42_000` → `KeyExpired`，证明 verified_at 来自 AuthorityClock 而非调用方时间）、无界 correlation → 类型化 `INVALID_ARGUMENT` 失败 envelope。
7. **CLI `--auth` 变体登记为后续**（任务允许"若侵入大则登记"）：命令行出示 principal Ed25519 私钥需要 key custody 约定，而私钥保管权威在 ADR-0011/B-IDENTITY-001 中显式独立（"Private key custody ... remain separate authorities"）；在 production bin 的 argv 上即兴发明裸私钥 hex 约定属硬塞。库级 `dispatch_over_authenticated_socket` 已覆盖 `authenticated_connect` 客户端面（本测试套件即经其全链路验证），CLI flag 待 custody 决策后以同函数接入。

## 验证

验证环境：macOS（darwin，arm64），仓库 HEAD `2c424da`（叠加并行车道 wait-control / takeover-control 未提交改动，均在本写集之外、未触碰）。

- `cargo test -p nlos-system-control`：**31 passed / 0 failed**（lib 单测 5、bin 0、`control_command_cli` 3——既有 conformance 等价面全绿，零改动证明、`control_ipc_auth` 8（新）、`metrics_export_contract` 3、`recovery_control` 7、`system_control_failure_mapping` 5、`windows_named_pipe` 0（macOS 目标）、doc-tests 0）。
- `cargo clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：通过（0 warning / 0 error）。
- `cargo +nightly-2026-08-01 clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：通过。
- `cargo fmt -p nlos-system-control --check`（stable 1.97）与 `cargo +nightly-2026-08-01 fmt -p nlos-system-control --check`：均通过。
- `cargo check -p nlos-system-control --no-default-features`：通过（非 cli 形态不编译 auth 面）。

## 验证与边界

本证据为单节点本地 `PASS`。已知限制：

1. **连接级 ≠ 命令级身份绑定**：握手认证的是连接；verified principal 与每条命令 issuer 身份的密码学绑定是 ADR-0011 决定 2（命令级签名贯穿，ADR-0010 wire 化），归该实现切片。认证入口当前不比本地信任域路径多做 envelope 内身份检查（该路径本就由服务端 `SystemControlAuthorizer` 把守 policy）。
2. **每次连接一条 wall receipt**：握手 verified_at 以 nonce 派生键签发，pre-gate 拒绝的连接也会留下一条单调时钟 receipt（拒绝仍 fail-closed；nonce registry 零副作用不变）。速率受限场景可接受；若需配额可后续 additive。
3. **CLI `--auth` 未接**（见已实现事实 7）；`system-control-cli` 仍走本地信任域。
4. Windows named-pipe 认证适配未包含（`auth` 模块全量 `#[cfg(unix)]`，与既有 CLI 前缀同姿态）。
5. `Cargo.lock` 为共享再生工件：本写集贡献其中 `nlos-system-control` 依赖块（ed25519-dalek/nlos-clock/nlos-identity/sha2），`nlos-takeover-control`/`nlos-wait-control` 块属并行车道。
