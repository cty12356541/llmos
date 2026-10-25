# sample-app — 第三方样板应用（W33-B / B1-5 · W65-CAPP-PAYLOAD / 移交 #2）

一个完整的第三方应用样例，证明不触任何内核内部 API 的开发者可以只骑
公共面（`nlos-package` CLI + 各权威 crate 的公共 API + `nlos-slice-k`
装配）走完整个生命周期：

```text
开发（manifest + 载荷 + tasks 模板）
  → keygen → build → conformance → verify（签名 + 权威验签 receipt）
  → install（receipt digest-binding → ApplicationAuthority）
  → run（execute_application_payload 执行 app.bin 载荷回执
         + Task/Attempt/Process → fiber driver operation → Artifact + 回执）
  → update（同 major 新 revision → W29-E 迁移 runner / CLI update 原子切换）
  → payload（代际 2 的 v1.1.0 app.bin 载荷执行与回执复验）
  → uninstall（W27-D 活动门 → W30-D teardown 链 → 过门卸载）
```

证据文件：

- `docs/evidence/stage-b/b-application-007-third-party-sample.md`（ROAD-B-001 的 constructive exit 证据 + W35-P2 前片）
- `docs/evidence/stage-c/c-app-payload-001-sample-cli-execution.md`（阶段 C 移交 `#2 C-APP-PAYLOAD` 闭环证据：CLI 全子命令 + 样板载荷执行车道接线 + 跨平台集成验证）

开发者打包指南：`docs/developers/packaging.md`。

## 一键演练与无头测试

```sh
sh examples/sample-app/lifecycle.sh
```

脚本自带退出码与输出断言（任何一步失败即非零退出）：构建
`nlos-package` 与 `sample-app-driver`（若缺）、依次执行混合驱动生命周期（含代际 1/2 载荷执行回执与跨工具 replay 核对、W29-E 迁移、W27-D 活动门与 W30-D 真实子进程 kill 链）以及独立状态根上的纯 `nlos-package` CLI 全链闭环（`install → run → update → run → uninstall` + 负门拒绝）。

机械化的跨平台无头测试（CI 在 Linux / macOS / Windows 均可直接运行）：

- `cargo test -p nlos-package --test sample_app_cli_lifecycle`（真实 `nlos-package` CLI 二进制驱动本目录 `package/` v1.0.0 与 `package-v1.1.0/` v1.1.0 全生命周期、负门与确定性重放）
- `cargo test --manifest-path examples/sample-app/Cargo.toml`（`sample-app-driver` 进程内跨代际载荷执行与生命周期回归）
- `cargo test -p nlos-slice-k --test third_party_sample_lifecycle`（`nlos-slice-k` 权威装配层孪生测试）

## 目录

```text
sample-app/
├── package/            # 开发者目录 v1.0.0（manifest + 载荷 + tasks/ 模板体）
├── package-v1.1.0/     # 同一目录在版本演进后的快照（同 package-id，版本 1.1.0）
├── src/main.rs         # sample-app-driver：生命周期驱动（只 import 公共 API）
├── lifecycle.sh        # 全生命周期演练脚本（真实 CLI + 驱动 + 子进程 kill 链）
└── Cargo.toml          # 独立 workspace 根（desktop/ 先例；不入仓库 workspace）
```

`package.manifest` 声明 `entry = sample-driver executable app.bin` 与
`entry = sample-config data assets/config.bin`，其 `tasks` 段演练 W28-B
模板面：两个模板（`agent-role` 主节点 + 依赖它的 `executable` 服务节点），
十个 digest 体文件在 `tasks/` 下；构建走 with-tasks 域分隔签名面，验证走
`verify_package_with_tasks`。

## 分步命令

### 路径 A：`sample-app-driver` + `nlos-package` 完整运行时生命周期

（`<repo>` 为仓库根；`STATE` 为任一空目录，权威们会在其下落库。）

```sh
# 0. 工具链
cargo build -p nlos-package                                     # → target/debug/nlos-package
cargo build --manifest-path examples/sample-app/Cargo.toml      # → examples/sample-app/target/debug/sample-app-driver

# 1. 密钥（熵由开发者供给；样例种子见 lifecycle.sh，勿用于生产）
openssl rand -hex 32
nlos-package keygen --seed <HEX64> --out sample-app.devkey

# 2-3. 构建 + 合规检查 + 权威验签
nlos-package build examples/sample-app/package --key sample-app.devkey --out app-v1.nlospkg
nlos-package conformance app-v1.nlospkg
nlos-package verify app-v1.nlospkg --store $STATE/artifacts --identity $STATE/identity
# → VERIFIED <receipt-id-HEX32>

# 4. 安装（驱动只拿 receipt id；内部走 install_application 的七项 digest 绑定）
sample-app-driver install $STATE <receipt-id>

# 5. 运行（先经 execute_application_payload 执行 sample-driver / app.bin 并输出
#    payload_operation_id / payload_generation / payload_digest 回执；再走 Task/Attempt
#    → Process → CommitPermit → fiber operation + artifact stage/plan/converge →
#    background task 与 process binding 注册；Unix 下启动真实子进程充当 Os 替身）
sample-app-driver run $STATE <package-id>

# 6-7. 更新 + 代际 2 载荷复验
nlos-package build examples/sample-app/package-v1.1.0 --key sample-app.devkey --out app-v2.nlospkg
nlos-package verify app-v2.nlospkg --store $STATE/artifacts --identity $STATE/identity
sample-app-driver update $STATE <package-id> <receipt-id-v2>
sample-app-driver payload $STATE <package-id> sample-driver

# 8. 卸载（W27-D 活动门拒绝 → W30-D teardown 链 → 过门卸载）
sample-app-driver uninstall $STATE <package-id> <service_os_pid>
```

### 路径 B：纯 `nlos-package` CLI 开发者闭环

```sh
nlos-package install app-v1.nlospkg --root $CLI_STATE
nlos-package run 9a1c7d3e5f0b2a4c6d8e0f1a2b3c4d5e sample-driver --root $CLI_STATE
nlos-package update app-v2.nlospkg --root $CLI_STATE --compat same-major
nlos-package run 9a1c7d3e5f0b2a4c6d8e0f1a2b3c4d5e sample-driver --root $CLI_STATE
nlos-package uninstall 9a1c7d3e5f0b2a4c6d8e0f1a2b3c4d5e --root $CLI_STATE
```

## 诚实范围

- 载荷执行面（W35-P2 + W38-P2 + W65-CAPP-PAYLOAD 已闭环）：`sample-app-driver run`（及 `payload` 子命令）与 `nlos-package run` 均真实接入 `nlos_slice_k::execute_application_payload`——从 `ArtifactStore` 读取当前安装代际 `sample-driver`（`app.bin`）的物化字节，以载荷字节域分隔摘要为完成种子驱动 `nlos-driver-mock` 的 `register → dispatch → complete` 三边界持久回执，并经 `SemanticWriter` 写入一条授权语义断言回执。**诚实边界**：该车道验证的是内核持久 `Operation`、制品摘要绑定与语义回执桥接链路，并非任意客户机 Wasm / ELF / 原生沙箱指令虚拟机；同时验签/安装回执持久化的是签名后的 `manifest_digest`，未单独落表保存逐 entry 的 `PackageEntryRole`（CLI `install`/`update` 从包文件解析并列出 `executables sample-driver`，运行时按 `derive_artifact_id(package_id, version, entry_name)` 解析已物化制品并拒绝未声明入口或裸文件路径如 `assets/config.bin`）。
- `background-service` 的 Os 侧替身在 Unix 下是 `sleep 600` 真实子进程（slice-k 测试同款纪律），经 supervisor registry 注册、被真实 `SIGTERM` 杀死；非 Unix 宿主走 `NoopPlatformKillAdapter` 契约通道；它不是内核托管的服务进程。
- 信任边界沿用 `nlos-package` 的声明：verify 证明「签名与密钥一致」，不是信任决策；生产密钥托管/trust root 是部署侧关注点。
- 密钥/时间戳纪律：slice-k 复用助手按 seed 带；驱动自有键走 `llmos/sample-app/driver-key/v1` 域分隔派生（见 `src/main.rs` 注释）。
