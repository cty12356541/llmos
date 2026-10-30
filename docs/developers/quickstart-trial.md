# NLOS 本地开箱真实试用指南（Stage E `E0` / `W77-PRE`）

> 状态：`DESIGN+IMPLEMENTED`（`W77-PRE` / `E0` `E-TRIAL-BOOTSTRAP`；样板应用与试用打包证据见 [E-TRIAL-002](../evidence/stage-e/e-trial-002-quickstart-and-starter-app.md)，用户态 LLM HTTP 驱动证据见同波 `docs/evidence/stage-e/e-trial-001-llm-http-driver.md`）
>
> 适用范围：Windows 10/11（PowerShell 5.1+ / PowerShell 7+）与 macOS / Linux（POSIX `sh` / `bash`）本地开发工作站。

本指南面向希望在单台开发工作站上从零跑通 NLOS 控制面、`Z0–Z4` 工业工作站桌面、核外用户态真实大模型 HTTP 驱动（含本机 `~/glm-keypool` 号池网关或本地 Ollama）、多步生产力样板应用 [starter-assistant-app](../../examples/starter-assistant-app/README.md) 以及本地未公证试用桌面安装包的开发者。

---

## 0. 前置工具链要求与架构安全红线

### 0.1 本地环境要求

| 工具 | 最低版本要求 | 用途 |
|---|---|---|
| Rust (`cargo` / `rustc`) | `1.97+`（Edition 2024） | 编译 NLOS 核心权威 crate、`nlos-up`、`nlos-driver-llm-http`、`nlos-package` 与 Tauri 宿主 |
| Node.js / `npm` | `Node.js >= 20` | 构建 `desktop/` TypeScript + Vite 前端与调用 `@tauri-apps/cli` |
| Python | `3.10+`（可选） | 运行文档与仓库卫生自检脚本 |

可通过桌面试用脚本的预检模式一键核验本机工具链（不执行耗时编译）：

- **Windows PowerShell**：
  ```powershell
  powershell -ExecutionPolicy Bypass -File desktop/scripts/build-trial-bundle.ps1 -CheckOnly
  ```
- **macOS / Linux POSIX Shell**：
  ```sh
  sh desktop/scripts/build-trial-bundle.sh --check-only
  ```

### 0.2 微内核与凭据安全边界

1. **微内核零厂商 API 侵入（`KABI-SVC-001`）**：微内核与核心权威绝不内嵌任何大模型厂商 HTTP SDK 或特定协议；一切外部/本地大模型调用均运行于核外用户态驱动平面 [crates/nlos-driver-llm-http](../../crates/nlos-driver-llm-http/Cargo.toml)。
2. **凭据隔离与五维预算门控（`DRV-CRED-001` / `DRV-SHIM-001` / `TCB-EGRESS-001`）**：外部 API Key / 号池 Egress Key 仅由可信 `EnforcementShim` 托管并在单次授权出站时注入，不可信应用负载零原始密钥可见，且每次模型调用均受 `ResourceAuthority` v8 五维 `LlmTokenBudget`（`prompt_tokens`、`completion_tokens`、`total_tokens`、`api_calls`、`cost_microcredits`）整数上限约束。
3. **零密钥入库纪律（`[PD-COMMIT-004]`）**：严禁将任何真实 API Key 或号池密钥写入受 Git 跟踪的配置文件、清单或脚本；本文档统一使用 `<YOUR_EGRESS_KEY>` 作为占位符。

---

## 1. 一键初始化与启动（`nlos-up daemon` / `desktop`）

一键引导启动器 [crates/nlos-up](../../crates/nlos-up/Cargo.toml) 负责幂等初始化本机状态根（POSIX 默认 `~/.llmos`，Windows 默认 `$env:USERPROFILE\.llmos`）、生成本机 Ed25519 身份与 operator 会话密钥、在跨平台本地 IPC 端点（Unix Domain Socket 或 Windows Named Pipe）拉起控制面守护进程，并向 [desktop/](../../desktop/README.md) 自动注入 `LLMOS_DESKTOP_*` 会话参数。

### 1.1 仅启动后台控制面守护进程（`daemon` 模式）

适用于通过 CLI、SDK 或独立终端联调控制面服务：

- **Windows PowerShell**：
  ```powershell
  # 默认初始化并复用 $env:USERPROFILE\.llmos
  cargo run -p nlos-up -- daemon

  # 或指定隔离试用状态目录
  $TrialRoot = Join-Path $env:TEMP "llmos-trial-state"
  cargo run -p nlos-up -- daemon --state-dir $TrialRoot
  ```

- **macOS / Linux POSIX Shell**：
  ```sh
  # 默认初始化并复用 ~/.llmos
  cargo run -p nlos-up -- daemon

  # 或指定隔离试用状态目录
  TRIAL_ROOT="${TMPDIR:-/tmp}/llmos-trial-state"
  cargo run -p nlos-up -- daemon --state-dir "$TRIAL_ROOT"
  ```

### 1.2 一键拉起守护进程与 `Z0–Z4` 工业工作站桌面（`desktop` 模式）

自动完成状态根引导、后台控制面就绪探测与 Tauri 桌面壳会话绑定（含 Windows Named Pipe 与 Unix Socket 双平台开发夹具）：

- **Windows PowerShell**：
  ```powershell
  cargo run -p nlos-up -- desktop
  ```

- **macOS / Linux POSIX Shell**：
  ```sh
  cargo run -p nlos-up -- desktop
  ```

> 若仅需单独调试前端与桌面宿主开发夹具，也可参见 [desktop/README.md](../../desktop/README.md) 在 `desktop/` 目录运行 `npm run dev:server` 与 `npx tauri dev`。

---

## 2. 本机 `~/glm-keypool` 自动发现与 `nlos-llm-pool-cli` 直连测试

用户态模型驱动 [crates/nlos-driver-llm-http](../../crates/nlos-driver-llm-http/Cargo.toml)（配套命令行探针 `nlos-llm-pool-cli`）支持自动发现本机 `~/glm-keypool`（Windows 下为 `$env:USERPROFILE\glm-keypool`）代理配置，默认对接本机 OpenAI-compatible 编码网关 `http://127.0.0.1:8317/api/coding/paas/v4`；同时也支持显式环境变量或参数对接本地 Ollama（如 `http://127.0.0.1:11434/v1`）及其他兼容端点。

### 2.1 配置出站凭据占位（零明文落库）

若本机已运行 `~/glm-keypool` 代理服务，驱动会按本机工作目录或用户主目录下的 `~/glm-keypool` 运行态配置执行只读自动发现（密钥仅载入 `EnforcementShim` 内存托管区，绝不打印或写入日志）。也可在当前终端会话中显式注入占位变量覆盖：

- **Windows PowerShell**：
  ```powershell
  $env:NLOS_LLM_BASE_URL = "http://127.0.0.1:8317/api/coding/paas/v4"
  $env:NLOS_LLM_EGRESS_KEY = "<YOUR_EGRESS_KEY>"
  ```

- **macOS / Linux POSIX Shell**：
  ```sh
  export NLOS_LLM_BASE_URL="http://127.0.0.1:8317/api/coding/paas/v4"
  export NLOS_LLM_EGRESS_KEY="<YOUR_EGRESS_KEY>"
  ```

### 2.2 执行 `status` / `chat` / `intent` 三步直连验证

1. **查看端点自动发现、凭据掩码状态与五维 Token 预算余量（`status`）**：
   - **Windows PowerShell**：
     ```powershell
     cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- status
     ```
   - **macOS / Linux POSIX Shell**：
     ```sh
     cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- status
     ```

2. **执行受五维 `LlmTokenBudget` 记账保护的单轮对话推理（`chat`）**：
   - **Windows PowerShell**：
     ```powershell
     cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- chat `
       --prompt "Summarize the three core invariants of NLOS ArtifactStore provenance."
     ```
   - **macOS / Linux POSIX Shell**：
     ```sh
     cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- chat \
       --prompt "Summarize the three core invariants of NLOS ArtifactStore provenance."
     ```

3. **验证桌面命令面板自然语言意图结构化解析桥（`intent`）**：
   - **Windows PowerShell**：
     ```powershell
     cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- intent `
       --utterance "Create a two-step release readiness brief and inspect token budget usage"
     ```
   - **macOS / Linux POSIX Shell**：
     ```sh
     cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- intent \
       --utterance "Create a two-step release readiness brief and inspect token budget usage"
     ```

每次调用完成后，`EnforcementShim` 会将模型返回的 `usage` 字段同步至 `ResourceAuthority::consume_llm_tokens`，产出带单调递增序号与幂等键绑定的不可变 `LlmTokenConsumptionReceipt`。

---

## 3. `starter-assistant-app` 签名打包、一致性门禁与多步 `TaskPlan` + `Artifact` 运行

内置样板应用 [examples/starter-assistant-app](../../examples/starter-assistant-app/README.md) 提供了完整的 `v1.0.0`（[package/package.manifest](../../examples/starter-assistant-app/package/package.manifest)）与 `v1.1.0`（[package-v1.1.0/package.manifest](../../examples/starter-assistant-app/package-v1.1.0/package.manifest)）包树，包含可执行载荷 `assistant-driver`、配置数据载荷 `assistant-profile` 以及串行依赖的 `TaskPlan` 节点模板（`draft-plan -> synthesize-artifact`，并在 `v1.1.0` 扩展 `verify-brief`）。详细包格式参考见 [packaging.md](./packaging.md)。

### 3.1 签名打包、`conformance` 门禁与 `nlos-package` CLI 生命周期

注意：始终将 `.devkey` 与 `.nlospkg` 输出至临时目录并在试用后清理，禁止将二进制 `.nlospkg` 提交入库。

- **Windows PowerShell**：
  ```powershell
  $PkgTmp = Join-Path $env:TEMP "nlos-starter-quickstart"
  New-Item -ItemType Directory -Force -Path $PkgTmp | Out-Null

  # 1. 生成确定性试用签名密钥（仅限本机开发测试）
  cargo run -p nlos-package -- keygen `
    --seed 4747474747474747474747474747474747474747474747474747474747474747 `
    --out "$PkgTmp\starter.devkey"

  # 2. 打包 v1.0.0 与 v1.1.0 并通过 20 项静态一致性门禁（输出 CONFORMANT）
  cargo run -p nlos-package -- build examples/starter-assistant-app/package `
    --key "$PkgTmp\starter.devkey" `
    --out "$PkgTmp\starter-v1.0.0.nlospkg"
  cargo run -p nlos-package -- conformance "$PkgTmp\starter-v1.0.0.nlospkg"

  cargo run -p nlos-package -- build examples/starter-assistant-app/package-v1.1.0 `
    --key "$PkgTmp\starter.devkey" `
    --out "$PkgTmp\starter-v1.1.0.nlospkg"
  cargo run -p nlos-package -- conformance "$PkgTmp\starter-v1.1.0.nlospkg"

  # 3. 驱动完整安装 -> 执行 -> SameMajor 升级 -> 执行 -> 卸载闭环
  $CliRoot = Join-Path $PkgTmp "state"
  cargo run -p nlos-package -- install "$PkgTmp\starter-v1.0.0.nlospkg" --root $CliRoot
  cargo run -p nlos-package -- run 4e1077c1a5b24d8e9f0123456789abcd assistant-driver --root $CliRoot
  cargo run -p nlos-package -- update "$PkgTmp\starter-v1.1.0.nlospkg" --root $CliRoot --compat same-major
  cargo run -p nlos-package -- run 4e1077c1a5b24d8e9f0123456789abcd assistant-driver --root $CliRoot
  cargo run -p nlos-package -- uninstall 4e1077c1a5b24d8e9f0123456789abcd --root $CliRoot

  Remove-Item -Recurse -Force $PkgTmp
  ```

- **macOS / Linux POSIX Shell**：
  ```sh
  PKG_TMP="$(mktemp -d "${TMPDIR:-/tmp}/nlos-starter-quickstart.XXXXXX")"

  cargo run -p nlos-package -- keygen \
    --seed 4747474747474747474747474747474747474747474747474747474747474747 \
    --out "$PKG_TMP/starter.devkey"

  cargo run -p nlos-package -- build examples/starter-assistant-app/package \
    --key "$PKG_TMP/starter.devkey" \
    --out "$PKG_TMP/starter-v1.0.0.nlospkg"
  cargo run -p nlos-package -- conformance "$PKG_TMP/starter-v1.0.0.nlospkg"

  cargo run -p nlos-package -- build examples/starter-assistant-app/package-v1.1.0 \
    --key "$PKG_TMP/starter.devkey" \
    --out "$PKG_TMP/starter-v1.1.0.nlospkg"
  cargo run -p nlos-package -- conformance "$PKG_TMP/starter-v1.1.0.nlospkg"

  CLI_ROOT="$PKG_TMP/state"
  cargo run -p nlos-package -- install "$PKG_TMP/starter-v1.0.0.nlospkg" --root "$CLI_ROOT"
  cargo run -p nlos-package -- run 4e1077c1a5b24d8e9f0123456789abcd assistant-driver --root "$CLI_ROOT"
  cargo run -p nlos-package -- update "$PKG_TMP/starter-v1.1.0.nlospkg" --root "$CLI_ROOT" --compat same-major
  cargo run -p nlos-package -- run 4e1077c1a5b24d8e9f0123456789abcd assistant-driver --root "$CLI_ROOT"
  cargo run -p nlos-package -- uninstall 4e1077c1a5b24d8e9f0123456789abcd --root "$CLI_ROOT"

  rm -rf "$PKG_TMP"
  ```

### 3.2 运行端到端多步 `TaskPlan` + 五维 `LlmTokenBudget` + `Artifact` 因果血缘工作流

直接运行 [examples/starter-assistant-app/src/main.rs](../../examples/starter-assistant-app/src/main.rs) 将在进程内完整驱动全部公共权威并校验每一步持久回执：

- **Windows PowerShell**：
  ```powershell
  cargo test --manifest-path examples/starter-assistant-app/Cargo.toml
  cargo run --manifest-path examples/starter-assistant-app/Cargo.toml
  ```

- **macOS / Linux POSIX Shell**：
  ```sh
  cargo test --manifest-path examples/starter-assistant-app/Cargo.toml
  cargo run --manifest-path examples/starter-assistant-app/Cargo.toml
  ```

一次标准运行会依次完成并打印以下可核验回执：

1. `INSTALL_V1` + `PAYLOAD_V1`：验签安装 `v1.0.0`（`generation = 1`），实例化 `TaskPlan` `revision = 1`（2 个节点），并执行代际 1 `assistant-driver` 载荷；
2. `BUDGET_INIT`：在 `ResourceAuthority` 创建账户与五维 `LlmTokenBudget`（`max_total_tokens = 16384`、`max_api_calls = 8`、`max_cost_microcredits = 50000`）；
3. `PLAN_GATE_BLOCKED`：验证 `[PLAN-LAZY-001]` 依赖围栏——在 `draft-plan` 完成前请求物化 `synthesize-artifact` 会确定性返回 `DependenciesNotReady`；
4. `LLM_RECEIPT` (`seq=1,2`) + `ARTIFACT_REV` (`revision=1,2`)：顺序推进 `draft-plan` 与 `synthesize-artifact` 至 `Completed`，分步扣减 Token 预算并把每个节点的 `(TaskNodeId, LlmTokenConsumptionReceiptId, UpstreamContentDigest)` 写入 `ArtifactStore` 的 `ProvenanceSourceTriple`；
5. `UPDATE_V1_1` + `PLAN_REV2` + `LLM_RECEIPT` (`seq=3`) + `ARTIFACT_REV` (`revision=3`)：执行 `v1.0.0 -> v1.1.0` `CompatibilityWindow::SameMajor` 热升级（`generation = 2`），在不改写前两个已冻结节点摘要的前提下应用 `TaskPlan` `revision = 2` 新增 `verify-brief` 节点并产出简报修订版 3；
6. `UNINSTALL` + `SUMMARY`：干净卸载应用并验证卸载后再次执行载荷被 fail-closed 拒绝。

---

## 4. `build-trial-bundle` 本地试用桌面打包与 `E-deferred` 商业公证边界

仓库跟踪的 `desktop/src-tauri/tauri.conf.json` 默认保持 `"bundle": { "active": false }`，以避免日常 `cargo test` / 开发编译产生庞大安装包开销。当开发者需要在本机生成可安装的桌面试用包时，请使用专用构建脚本 [desktop/scripts/build-trial-bundle.ps1](../../desktop/scripts/build-trial-bundle.ps1)（Windows）或 [desktop/scripts/build-trial-bundle.sh](../../desktop/scripts/build-trial-bundle.sh)（macOS / Linux）。

### 4.1 预检模式与完整本地打包命令

- **Windows PowerShell**：
  ```powershell
  # 1. 快速环境与配置完备性校验（不执行前端/Rust 编译，退出码 0）
  powershell -ExecutionPolicy Bypass -File desktop/scripts/build-trial-bundle.ps1 -CheckOnly

  # 2. 完整构建本机未公证试用桌面包（自动执行 npm ci/install -> npm run build -> npx tauri build --no-sign）
  powershell -ExecutionPolicy Bypass -File desktop/scripts/build-trial-bundle.ps1
  ```

- **macOS / Linux POSIX Shell**：
  ```sh
  # 1. 快速环境与配置完备性校验（不执行前端/Rust 编译，退出码 0）
  sh desktop/scripts/build-trial-bundle.sh --check-only

  # 2. 完整构建本机未公证试用桌面包（自动执行 npm ci/install -> npm run build -> npx tauri build --no-sign）
  sh desktop/scripts/build-trial-bundle.sh
  ```

脚本通过系统临时目录中的一次性 JSON 覆盖配置临时开启 `bundle.active = true` 并绑定 `icons/icon.png` 与 `icons/icon.ico`，退出时自动删除临时覆盖文件，保证工作区跟踪文件零污染。构建产物默认位于 `desktop/src-tauri/target/release/bundle/`。

### 4.2 商业代码签名与公证的诚实边界（`E-deferred` / `RISK-B-06`）

依据 [06-架构设计总纲-v0.5.md](../design/06-架构设计总纲-v0.5.md) §29、[stage-e-progress.md](../management/stage-e-progress.md) §E.1.3 / §E.2.3 与 [ADR-0027](../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md)：

> **诚实边界声明**：`build-trial-bundle.{ps1,sh}` 生成的桌面产物**仅限本机开发试用，未经过 Apple Developer ID / Microsoft EV 硬件令牌商业 CA 公证签名，商业发行签名维持在 `E-deferred` / `RISK-B-06`**。

具体区分如下：

1. **NLOS 内部包完整性签名**（`nlos-package` Ed25519 签名与阶段 E `E4` `nlos-publisher-trust` 发布者证书链）用于保护 `.nlospkg` 内容寻址制品与任务模板免受篡改，**不等同于**宿主操作系统（Windows SmartScreen / macOS Gatekeeper）的商业发行公证。
2. 在其他机器上分发未经商业 CA 硬件令牌签名公证的宿主安装包，会被 macOS Gatekeeper 或 Windows SmartScreen 拦截；外部商业 CA 付费账户与物理 USB 硬件令牌公证属于发行工程凭据门，在阶段 E 中诚实保留于 `E-deferred` / `RISK-B-06`，严禁以本地 `--no-sign` 试用包冒充已公证商业发行物。
