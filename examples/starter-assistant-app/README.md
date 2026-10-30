# starter-assistant-app — 开箱参考生产力样板应用（W77-PRE-C / E0）

阶段 E `E0`（`E-TRIAL-BOOTSTRAP`）内置的开箱参考生产力样板应用包，证明第三方生产力应用仅依赖 NLOS 公共权威 crate（`nlos-application`、`nlos-artifact`、`nlos-identity`、`nlos-plan`、`nlos-resource`、`nlos-slice-k`、`nlos-task`、`nlos-types`）与 `nlos-package` CLI 即可完成以下完整闭环，零内核私有 API 依赖：

```text
开发者包树（package/ v1.0.0 与 package-v1.1.0/ v1.1.0）
  -> 签名构建 + check_package_file / nlos-package conformance 一致性门禁
  -> 权威验签（verify_package_with_tasks -> PackageVerificationReceipt）
  -> 安装与模板编译（ApplicationAuthority gen 1 + SqlitePlanAuthority TaskPlan rev 1）
  -> 建立 ResourceAuthority 五维 LlmTokenBudget 并按步扣减生成不可变 LlmTokenConsumptionReceipt
  -> 推进多节点 TaskPlan DAG（[PLAN-LAZY-001] 依赖围栏 + Materializing -> Active -> Completed）
  -> 写入/修订内容寻址 ArtifactStore 简报（3 个修订版均绑定因果血缘 ProvenanceSourceTriple）
  -> v1.0.0 -> v1.1.0 CompatibilityWindow::SameMajor 热升级与 [PLAN-DAG-001] G1 冻结节点扩展（TaskPlan rev 2）
  -> 干净卸载与卸载后载荷执行 fail-closed 拒绝
```

相关文档：

- 开发者快速试用指南：[docs/developers/quickstart-trial.md](../../docs/developers/quickstart-trial.md)
- 第三方应用打包指南：[docs/developers/packaging.md](../../docs/developers/packaging.md)
- 基础生命周期样板对照：[examples/sample-app/README.md](../sample-app/README.md)
- 阶段 E `E0` 闭环证据：[docs/evidence/stage-e/e-trial-002-quickstart-and-starter-app.md](../../docs/evidence/stage-e/e-trial-002-quickstart-and-starter-app.md)

## 目录结构

```text
starter-assistant-app/
├── Cargo.toml             # 独立 [workspace] 根（unsafe_code = "forbid"，clippy::pedantic = "warn"）
├── Cargo.lock             # 独立样板锁文件（不触碰仓库根 Cargo.toml / Cargo.lock）
├── README.md              # 本说明文档
├── src/
│   └── main.rs            # 端到端生产力工作流驱动与单元/集成测试套件
├── package/               # v1.0.0 包树（2 个载荷 entry + 2 个串行依赖 task 模板）
│   ├── package.manifest
│   ├── assistant.bin
│   ├── assets/
│   │   └── assistant-profile.txt
│   └── tasks/
│       ├── draft-plan-{binding,inputs,outputs,policy,ceiling}.txt
│       └── synthesize-artifact-{binding,inputs,outputs,policy,ceiling}.txt
└── package-v1.1.0/        # v1.1.0 热升级包树（保留上游冻结节点摘要 + 新增第 3 节点 verify-brief）
    ├── package.manifest
    ├── assistant.bin
    ├── assets/
    │   └── assistant-profile.txt
    └── tasks/
        ├── draft-plan-{binding,inputs,outputs,policy,ceiling}.txt
        ├── synthesize-artifact-{binding,inputs,outputs,policy,ceiling}.txt
        └── verify-brief-{binding,inputs,outputs,policy,ceiling}.txt
```

清单文件 [package/package.manifest](./package/package.manifest) 与 [package-v1.1.0/package.manifest](./package-v1.1.0/package.manifest) 共享固定 `package-id = 4e1077c1a5b24d8e9f0123456789abcd`，声明可执行入口 `assistant-driver`（`assistant.bin`）与数据载荷 `assistant-profile`（`assets/assistant-profile.txt`）：

- `v1.0.0` 声明 2 个串行依赖节点：`draft-plan`（`10a1c2d3e4f5061728394a5b6c7d8e01`）-> `synthesize-artifact`（`20b2c3d4e5f60718293a4b5c6d7e8f02`）；
- `v1.1.0` 在字节级保持前两个已跨执行边界节点的五项摘要不变（遵守 `[PLAN-DAG-001]` G1 冻结节点契约），并在末端追加依赖 `synthesize-artifact` 的审计节点 `verify-brief`（`30c3d4e5f60718293a4b5c6d7e8f0303`）。

## 一键运行与测试

在仓库根目录执行：

```sh
# 1. 格式与严格 Clippy（含 pedantic）门禁
cargo fmt --manifest-path examples/starter-assistant-app/Cargo.toml -- --check
cargo clippy --manifest-path examples/starter-assistant-app/Cargo.toml --all-targets -- -D warnings

# 2. 单元与端到端集成测试（含预算耗尽、SameMinor 拒绝、G1 冻结节点改写拒绝等负路径）
cargo test --manifest-path examples/starter-assistant-app/Cargo.toml

# 3. 运行完整生产力工作流（默认使用临时状态目录并在退出时自动清理）
cargo run --manifest-path examples/starter-assistant-app/Cargo.toml

# 4. 可选：将权威数据库持久化到指定目录以便事后检查
cargo run --manifest-path examples/starter-assistant-app/Cargo.toml -- --root <STATE_DIR>
```

运行成功后控制台按序输出结构化持久回执摘要（`INSTALL_V1`、`PAYLOAD_V1`、`BUDGET_INIT`、`PLAN_GATE_BLOCKED`、`LLM_RECEIPT`、`ARTIFACT_REV`、`UPDATE_V1_1`、`PLAN_REV2`、`UNINSTALL`、`SUMMARY`）。

## 使用 `nlos-package` CLI 打包、校验与驱动生命周期

`package/` 与 `package-v1.1.0/` 完全兼容 `nlos-package` 开发者 CLI。注意请将构建出的 `.nlospkg` 输出至系统临时目录，切勿将二进制包提交入库。

### Windows PowerShell

```powershell
$PkgTmp = Join-Path $env:TEMP "nlos-starter-trial"
New-Item -ItemType Directory -Force -Path $PkgTmp | Out-Null

# 1. 生成本地试用签名密钥（仅限本机开发测试，勿用于生产）
cargo run -p nlos-package -- keygen `
  --seed 4747474747474747474747474747474747474747474747474747474747474747 `
  --out "$PkgTmp\starter.devkey"

# 2. 构建 v1.0.0 与 v1.1.0 签名包并执行 conformance 一致性门禁
cargo run -p nlos-package -- build examples/starter-assistant-app/package `
  --key "$PkgTmp\starter.devkey" `
  --out "$PkgTmp\starter-v1.0.0.nlospkg"
cargo run -p nlos-package -- conformance "$PkgTmp\starter-v1.0.0.nlospkg"

cargo run -p nlos-package -- build examples/starter-assistant-app/package-v1.1.0 `
  --key "$PkgTmp\starter.devkey" `
  --out "$PkgTmp\starter-v1.1.0.nlospkg"
cargo run -p nlos-package -- conformance "$PkgTmp\starter-v1.1.0.nlospkg"

# 3. CLI 全生命周期：install -> run -> update (same-major) -> run -> uninstall
$CliState = Join-Path $PkgTmp "cli-state"
cargo run -p nlos-package -- install "$PkgTmp\starter-v1.0.0.nlospkg" --root $CliState
cargo run -p nlos-package -- run 4e1077c1a5b24d8e9f0123456789abcd assistant-driver --root $CliState
cargo run -p nlos-package -- update "$PkgTmp\starter-v1.1.0.nlospkg" --root $CliState --compat same-major
cargo run -p nlos-package -- run 4e1077c1a5b24d8e9f0123456789abcd assistant-driver --root $CliState
cargo run -p nlos-package -- uninstall 4e1077c1a5b24d8e9f0123456789abcd --root $CliState

# 4. 清理临时二进制包与试用状态目录
Remove-Item -Recurse -Force $PkgTmp
```

### macOS / Linux POSIX Shell

```sh
PKG_TMP="$(mktemp -d "${TMPDIR:-/tmp}/nlos-starter-trial.XXXXXX")"

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

CLI_STATE="$PKG_TMP/cli-state"
cargo run -p nlos-package -- install "$PKG_TMP/starter-v1.0.0.nlospkg" --root "$CLI_STATE"
cargo run -p nlos-package -- run 4e1077c1a5b24d8e9f0123456789abcd assistant-driver --root "$CLI_STATE"
cargo run -p nlos-package -- update "$PKG_TMP/starter-v1.1.0.nlospkg" --root "$CLI_STATE" --compat same-major
cargo run -p nlos-package -- run 4e1077c1a5b24d8e9f0123456789abcd assistant-driver --root "$CLI_STATE"
cargo run -p nlos-package -- uninstall 4e1077c1a5b24d8e9f0123456789abcd --root "$CLI_STATE"

rm -rf "$PKG_TMP"
```

## 诚实范围与架构边界

- **微内核与用户态模型驱动隔离（`KABI-SVC-001` / `DRV-CRED-001` / `DRV-SHIM-001`）**：`starter-assistant-app` 在无外部网络依赖的确定性模式下直接演练 `ResourceAuthority` v8 五维 `LlmTokenBudget`（`prompt_tokens`、`completion_tokens`、`total_tokens`、`api_calls`、`cost_microcredits`）整数扣减、幂等重放与耗尽拦截，确保 CI 与离线环境 100% 可复现；真实本机或远端大模型 HTTP 调用统一由核外用户态驱动平面 `crates/nlos-driver-llm-http`（如对接本机 `~/glm-keypool` `127.0.0.1:8317/api/coding/paas/v4` 或 Ollama）在 `EnforcementShim` 托管凭据下执行，微内核绝不内嵌任何厂商 HTTP SDK。
- **载荷执行与强沙箱边界（`U-9`）**：`execute_application_payload` 验证的是已安装代际可执行制品摘要绑定、持久 `Operation` 三边界回执与语义断言桥接；面向不可信社区第三方代码的跨平台「独立宿主 Process + Wasmtime WASI 白名单」双层强沙箱排入阶段 E `W80-E6A-WASM-SANDBOX`（`crates/nlos-wasm-sandbox`），在 `U-9` 正式闭合前不将本样板外推为不可信原生二进制沙箱隔离证明。
- **签名信任根边界（`RISK-B-06`）**：样板内置的确定性 Ed25519 开发者种子仅用于本地包签名与 `IdentityAuthority` 验签流水线自检；三级发布者证书链与吊销纪元归属阶段 E `W79-E4-PUBLISHER-TRUST`，宿主 OS 安装包商业 CA 硬件令牌公证维持在 `E-deferred` / `RISK-B-06`。
