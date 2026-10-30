# NLOS 本地开箱真实试用指南（Stage E `E0` / `W77-PRE`）

> 状态：`VERIFIED (H4)`（`W77-PRE` / `E0` `E-TRIAL-BOOTSTRAP`；用户态 LLM HTTP 驱动与本机 `glm-keypool` 实盘冒烟证据见 [E-TRIAL-001](../evidence/stage-e/e-trial-001-llm-http-driver.md)，样板应用与试用打包证据见 [E-TRIAL-002](../evidence/stage-e/e-trial-002-quickstart-and-starter-app.md)）
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

## 1. 一键初始化与启动（`nlos-up init | env | daemon | desktop`）

一键引导启动器 [crates/nlos-up](../../crates/nlos-up/Cargo.toml) 负责幂等初始化本机状态根（POSIX 默认 `~/.llmos`，Windows 默认 `$env:USERPROFILE\.llmos`）、生成本机试用 Ed25519 身份与 operator 会话密钥、创建默认五维 `LlmTokenBudget` 账户（记录于 `<ROOT>/trial-account.txt`）、可选预装样板应用包（`--seed-sample-app`）、在跨平台本地 IPC 端点（Unix Domain Socket 或 Windows Named Pipe）拉起控制面守护进程，并向 [desktop/](../../desktop/README.md) 自动注入 `LLMOS_DESKTOP_*` 会话环境变量。

### 1.1 仅启动后台控制面守护进程（`daemon` 模式）

适用于通过 CLI、SDK 或独立终端联调控制面服务（也可先运行 `cargo run -p nlos-up -- init` 或 `cargo run -p nlos-up -- env` 仅执行状态根初始化与环境变量导出）：

- **Windows PowerShell**：
  ```powershell
  # 默认初始化并复用 $env:USERPROFILE\.llmos（可选附加 --seed-sample-app 预装样板应用）
  cargo run -p nlos-up -- daemon --seed-sample-app

  # 或指定隔离试用状态目录
  $TrialRoot = Join-Path $env:TEMP "llmos-trial-state"
  cargo run -p nlos-up -- daemon --root $TrialRoot --seed-sample-app
  ```

- **macOS / Linux POSIX Shell**：
  ```sh
  # 默认初始化并复用 ~/.llmos（可选附加 --seed-sample-app 预装样板应用）
  cargo run -p nlos-up -- daemon --seed-sample-app

  # 或指定隔离试用状态目录
  TRIAL_ROOT="${TMPDIR:-/tmp}/llmos-trial-state"
  cargo run -p nlos-up -- daemon --root "$TRIAL_ROOT" --seed-sample-app
  ```

### 1.2 一键拉起守护进程与 `Z0–Z4` 工业工作站桌面（`desktop` 模式）

自动完成状态根引导、后台控制面就绪探测与 Tauri 桌面壳会话绑定（同时支持 Windows Named Pipe 与 Unix Domain Socket；附加 `--dry-run` 可在不拉起 GUI 窗口的前提下核验全部注入环境变量与守护进程健康检查）：

- **Windows PowerShell**：
  ```powershell
  cargo run -p nlos-up -- desktop --seed-sample-app
  ```

- **macOS / Linux POSIX Shell**：
  ```sh
  cargo run -p nlos-up -- desktop --seed-sample-app
  ```

> 若仅需单独调试前端与桌面宿主开发夹具，也可参见 [desktop/README.md](../../desktop/README.md) 在 `desktop/` 目录运行 `npm run dev:server`（已在 `W77-PRE-A` 补齐 Windows Named Pipe 与 Unix Socket 双平台支持）与 `npm run tauri dev`。

### 1.3 桌面 `Ctrl+Space` 命令面板“模型辅助拆解 / Model Assist”卡片

通过 `nlos-up desktop` 进入工作站桌面后，按 **`Ctrl+Space`**（macOS 可用 `Cmd+Space`）或点击底部 `36px` 任务栏命令槽即可唤起 `Z3` 实心 `4px` 圆角命令与意图面板（[desktop/src/views/omnibar/palette-view.ts](../../desktop/src/views/omnibar/palette-view.ts) 与 [desktop/src-tauri/src/keypool_bridge.rs](../../desktop/src-tauri/src/keypool_bridge.rs)）：

1. **顶部实心 `4px` 本地模型池状态条**：自动调用 `llm_keypool_inspect_status` 探测本机 `~/glm-keypool`（或 `LLMOS_LLM_BASE_URL`），在线时显示 `本地模型池 · 在线`（`Local Model Pool · Online`）、默认模型与活跃通道数；未配置或网关离线时按 `[DUI-OFFLINE-001]` 自动切换为 `本地模型池 · 离线回退`（`Local Model Pool · Local Fallback`），绝不阻塞或锁死面板。
2. **非权威“模型辅助拆解 / Model Assist”卡片**：在输入框键入自然语言任务后点击 **`模型辅助拆解`**（英文界面为 **`Model Assist`**），后台线程经 `llm_keypool_assist_intent` 调用 `LlmEnforcementShim` 并自动扣减会话 `ResourceAuthority` 五维 `LlmTokenBudget`，在面板内渲染 `辅助拆解建议`（`Assisted Plan Breakdown`）卡片，展示建议步骤、可逆等级徽标与本次模型 Token 用量。
3. **高危歧义护栏与分层披露纪律**：凡输入包含破坏性关键词（如 `删除`、`卸载`、`清空`、`delete`、`wipe`、`uninstall`），无论模型原始输出为何或处于离线回退模式，均按 `[NL-AMBIG-001]` 强制升级为 `不可逆`（`Irreversible`）并开启候选澄清门，后续提交仍须经由 `ADR-0011` 控制脊柱与 `Z4` 实心安全确认屏审批；`L1/L2` 视图保持零内部架构行话，切换至 `L3` 诊断级别后可检视 `PROVIDER_ISOLATION_UNVERIFIED` 隔离标记与 16 位十六进制 `SHA-256` 脱敏槽位指纹。

---

## 2. 本机 `~/glm-keypool` 自动发现、`nlos-up` 一站式模型命令与 `nlos-llm-pool-cli` 直连测试

用户态模型驱动 [crates/nlos-driver-llm-http](../../crates/nlos-driver-llm-http/Cargo.toml) 与一键引导器 [crates/nlos-up](../../crates/nlos-up/Cargo.toml) 共享 4 层本地端点自动发现机制（显式 `--keypool-config` / `--pool-config` -> `LLMOS_KEYPOOL_CONFIG` -> `LLMOS_LLM_BASE_URL` -> `$USERPROFILE\glm-keypool\config.json` 或 `$HOME/glm-keypool/config.json`）。当检测到 `upstreamBase` 指向 `bigmodel.cn` 时，自动派生 OpenAI-compatible 编码网关路径 `http://127.0.0.1:8317/api/coding/paas/v4` 并选取 `glm-5.3` / `glm-5.3-flash`；同时也支持对接本地 Ollama（如 `http://127.0.0.1:11434/v1`）及其他兼容端点。

### 2.1 配置出站凭据占位（零明文落库）

若本机已运行 `~/glm-keypool` 代理服务，驱动会直接只读发现 `~/glm-keypool/config.json`（密钥仅封装于 `SecretCredential` 内存托管区，`Debug`/`Display` 恒输出 `<redacted-credential>`，且 `/__pool__/status` 槽位掩码与持有人标签 100% 剥离）。若未部署 `glm-keypool`，也可在当前终端会话中通过 `LLMOS_LLM_*` 环境变量直连任意兼容端点：

- **Windows PowerShell**：
  ```powershell
  $env:LLMOS_LLM_BASE_URL = "http://127.0.0.1:8317/api/coding/paas/v4"
  $env:LLMOS_LLM_API_KEY = "<YOUR_EGRESS_KEY>"
  $env:LLMOS_LLM_MODEL = "glm-5.3-flash"
  ```

- **macOS / Linux POSIX Shell**：
  ```sh
  export LLMOS_LLM_BASE_URL="http://127.0.0.1:8317/api/coding/paas/v4"
  export LLMOS_LLM_API_KEY="<YOUR_EGRESS_KEY>"
  export LLMOS_LLM_MODEL="glm-5.3-flash"
  ```

### 2.2 推荐方式：使用 `nlos-up pool-status | chat | intent` 一站式快捷命令（自动落库五维预算回执）

`nlos-up` 内置的模型池子命令会自动初始化或复用状态根（默认 `~/.llmos` / `$env:USERPROFILE\.llmos`，也可传 `--root <DIR>`）下的试用预算账户，在每次推理前后执行 `ResourceAuthority` 五维 `LlmTokenBudget` 预检与持久化回执记账：

- **Windows PowerShell**：
  ```powershell
  # 1. 探测本地模型池自动发现结果与在线健康状态（附加 --require-live 可在不可达时返回非零退出码）
  cargo run -p nlos-up -- pool-status
  cargo run -p nlos-up -- pool-status --require-live

  # 2. 直连本地模型池执行计量对话（自动归一化包含式 reasoning_tokens 并落盘 LlmTokenConsumptionReceipt）
  cargo run -p nlos-up -- chat --model glm-5.3-flash --max-tokens 1024 `
    --prompt "用一句话介绍 NLOS 微内核与能力令牌的核心价值"

  # 3. 自然语言意图结构化拆解（内置 [NL-AMBIG-001] 破坏性动作 IRREVERSIBLE 强制升级护栏）
  cargo run -p nlos-up -- intent --query "查看当前工作区正在运行的任务与剩余模型额度"
  cargo run -p nlos-up -- intent --query "卸载全部第三方应用并清空缓存"
  ```

- **macOS / Linux POSIX Shell**：
  ```sh
  # 1. 探测本地模型池自动发现结果与在线健康状态
  cargo run -p nlos-up -- pool-status
  cargo run -p nlos-up -- pool-status --require-live

  # 2. 直连本地模型池执行计量对话
  cargo run -p nlos-up -- chat --model glm-5.3-flash --max-tokens 1024 \
    --prompt "用一句话介绍 NLOS 微内核与能力令牌的核心价值"

  # 3. 自然语言意图结构化拆解
  cargo run -p nlos-up -- intent --query "查看当前工作区正在运行的任务与剩余模型额度"
  cargo run -p nlos-up -- intent --query "卸载全部第三方应用并清空缓存"
  ```

### 2.3 底层探针方式：使用 `nlos-llm-pool-cli` 执行无状态或指定 `--root` 直连验证

如需单独调试驱动 crate 而不触发完整 `nlos-up` 状态根初始化，可直接调用 `nlos-llm-pool-cli`（传入可选 `--root <STATE_DIR>` 时同样开启 `ResourceAuthority` 持久化五维记账）：

1. **查看端点自动发现、脱敏凭据状态与号池在线快照（`status`）**：
   - **Windows PowerShell / POSIX Shell**：
     ```sh
     cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- status
     ```

2. **执行单轮对话推理（`chat`）**：
   - **Windows PowerShell**：
     ```powershell
     cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- chat `
       --model glm-5.3-flash --max-tokens 1024 `
       --prompt "Summarize the three core invariants of NLOS ArtifactStore provenance."
     ```
   - **macOS / Linux POSIX Shell**：
     ```sh
     cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- chat \
       --model glm-5.3-flash --max-tokens 1024 \
       --prompt "Summarize the three core invariants of NLOS ArtifactStore provenance."
     ```

3. **验证自然语言意图结构化拆解与 `[NL-AMBIG-001]` 护栏（`intent`）**：
   - **Windows PowerShell**：
     ```powershell
     cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- intent `
       --query "Create a two-step release readiness brief and inspect token budget usage" `
       --locale en-US
     ```
   - **macOS / Linux POSIX Shell**：
     ```sh
     cargo run -p nlos-driver-llm-http --bin nlos-llm-pool-cli -- intent \
       --query "Create a two-step release readiness brief and inspect token budget usage" \
       --locale en-US
     ```

每次携带状态根调用完成后，`LlmEnforcementShim` 会按 OpenAI/BigModel 包含式思维链口径守恒折算 `prompt_tokens + completion_tokens + reasoning_tokens == total_tokens` 并提交 `ResourceAuthority::consume_llm_tokens`，产出带单调递增 `sequence`、幂等键绑定与 `PROVIDER_ISOLATION_UNVERIFIED` 诚实标签的不可变 `LlmTokenConsumptionReceipt`。

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
