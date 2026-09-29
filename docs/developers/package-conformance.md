# NLOS 包一致性 conformance 检查器（`nlos-package conformance`）

> 状态：`DESIGN+IMPLEMENTED`（W33-C / X-3 中；证据见 [B-ARTIFACT-007 §6](../evidence/stage-b/b-artifact-007-package-sdk.md)）
>
> 适用版本：`nlos/package-file/v1` · 库面 `nlos_artifact::check_package_file`

面向第三方包生产者的一致性检查器：对**任意** `nlos/package-file/v1` 包文件（不限于本 CLI 构建的包）离线运行一套带稳定规则号的规则集，逐条输出 typed finding。它与 `nlos-package verify` 互补而不重叠：

| | `verify` | `conformance` |
|---|---|---|
| 语义 | 内核同一条权威验签管线（fail-closed **准入**一个包） | 生产者侧**规则报告**（收集全部违规，不做准入） |
| 依赖 | 真实 `ArtifactStore` + `IdentityAuthority` | 零 store/零 identity，纯离线 |
| 输出 | VERIFIED/REPLAYED receipt（衔接安装链） | `PKG-CONF-###` findings + 汇总 |
| 失败 | 第一处失败即 typed 退出（3/4） | exit 6，报告可含多条 |

## 1. 用法与退出码

```text
$ nlos-package conformance <PKGFILE>
PKG-CONF-030 entry "hello": payload digest does not match the declared digest
NONCONFORMANT sample.nlospkg findings 1
$ echo $?
6
```

| 码 | 含义 |
|---|---|
| 0 | CONFORMANT（零 finding） |
| 1 | 用法错误 |
| 2 | 文件不可读 |
| 6 | NONCONFORMANT（finding 数 ≥ 1） |

结构类失败（magic/帧）会终止该次检查——更深层字段不可信；解码成功后的全部规则（schema、tasks 形状、签名链、摘要一致性、窗口与版本元数据）一次收集进同一份报告。库面等价入口：`nlos_artifact::check_package_file(&[u8]) -> ConformanceReport`（finding 带 `ConformanceRule`，`id()` 即规则号）。

## 2. 规则集（规则号是契约面，禁止重排）

「权威来源」说明规则出处：**格式** = `nlos/package-file/v1` 帧不变量；**验签路径** = 内核 verify 管线必然执行或依赖的约束；**构建惯例** = 文档声明的确定性构建约定（packaging.md §5）；**kit** = 面向生产者的元数据健全性要求（比内核准入更严，如实标注）。

### 结构 / 布局

| 规则号 | 检查 | 权威来源 | 说明 |
|---|---|---|---|
| `PKG-CONF-001` | magic `nlos/package-file/v1` | 格式 | 文件不以 magic 开头（含空文件/截断到 magic 之前） |
| `PKG-CONF-002` | 规范帧完整 | 格式 | 截断、签名后尾随字节、计数超出平台 usize |
| `PKG-CONF-003` | entry 名形状 | 格式 | 长度 ∈ [1,255] 字节、UTF-8、无 NUL |
| `PKG-CONF-004` | 枚举字节已知 | 格式 | entry role ∈ {1,2,3}、task kind ∈ {1,2} |

### manifest schema 完整性（含 tasks 模板段）

| 规则号 | 检查 | 权威来源 | 说明 |
|---|---|---|---|
| `PKG-CONF-010` | 至少一个 entry | 验签路径 | `validate_manifest` 同款约束（verify 必拒） |
| `PKG-CONF-011` | entry 名唯一 | 验签路径 | 同上 |
| `PKG-CONF-012` | 模板 node key 段内唯一 | 验签路径 | `validate_task_templates` 同款，逐规则展开 |
| `PKG-CONF-013` | 无自依赖 | 验签路径 | 同上 |
| `PKG-CONF-014` | 依赖引用同段已声明 key | 验签路径 | 同上（环检测仍由 plan authority 独占，`[PLAN-OVERRIDE-001]`） |
| `PKG-CONF-015` | 模板数 ≤ 100 000 | 验签路径 | `MAX_TASK_TEMPLATES_PER_MANIFEST` |
| `PKG-CONF-016` | 单模板依赖数 ≤ 256 | 验签路径 | `MAX_TASK_DEPENDENCIES_PER_TEMPLATE` |

kit 的 012–016 与 `validate_task_templates` 为同域两实现（一个给出权威判定、一个给出规则号粒度），一致性由测试钉死（`task_rules_agree_with_validate_task_templates`）。

### 签名链与摘要一致性

| 规则号 | 检查 | 权威来源 | 说明 |
|---|---|---|---|
| `PKG-CONF-020` | 签名链自洽 | 验签路径 | 64 字节签名须对**正确面**的域分隔 manifest 摘要（legacy / with-tasks，由是否存在 task 段决定）在内嵌公钥描述子上验证通过；公钥非合法 Ed25519 验证钥同样命中 |
| `PKG-CONF-030` | 载荷摘要一致 | 验签路径 | 每 entry `SHA-256(payload)` == 声明 digest（verify 侧 `PackageTampered` 的离线前哨） |
| `PKG-CONF-031` | artifact_id 按文档派生 | 构建惯例 | 每 entry `artifact_id == SHA-256("llmos/package-file/artifact-id/v1" ‖ package_id ‖ version ‖ name-len ‖ name)[..16]`（packaging.md §5 可复现构建） |

### 兼容窗 / 元数据健全性

| 规则号 | 检查 | 权威来源 | 说明 |
|---|---|---|---|
| `PKG-CONF-040` | 签名者密钥有效期窗非空 | 格式 | `valid_from_ms ≤ valid_until_ms`（解码层拒绝，kit 映射为规则号） |
| `PKG-CONF-041` | 有效期窗在 durable 台账界内 | 验签路径 | `valid_until_ms ≤ i64::MAX`——身份台账按 SQLite INTEGER 存毫秒窗，超界窗在 verify 时根本无法 bootstrap（keygen 已钉同一界） |
| `PKG-CONF-042` | 版本元数据非零 | kit | `version ≠ 0`：点分三元组打包公式下 0 即 `0.0.0`，无发布语义；更新兼容窗（SameMajor/SameMinor）比较的 major/minor 需要非零打包版本才有意义 |

## 3. 信任边界（如实声明）

- `PKG-CONF-020` 证明「签名与包内嵌公钥一致」，与 `verify` 的新鲜路径相同的第一步，但**不是**信任决策：principal 可达性、密钥吊销、trust root 均需部署侧身份权威，kit 刻意不携带（见 packaging.md §4 保管边界）。
- kit 可比内核准入更严（`PKG-CONF-031`/`042`）：不满足 kit 规则的包也许能通过 `verify`（artifact_id 任意值仍可物化），但不符合文档声明的可复现构建/发布元数据约定——这正是给第三方生产者的契约。

## 4. 已知限制

- 结构失败（001/002）终止检查：一次只报一个结构 finding；修复后重跑。
- 不含开发者目录布局检查（那是 `build` 的输入面，packaging.md §2）；kit 只检查产物文件。
- 042 为 kit 级要求，内核侧无对应拒绝路径（如实登记，见证据 §6）。

## 5. 相关

- [packaging.md](packaging.md)：W33-A 打包工具链（keygen/build/verify）
- [B-ARTIFACT-007](../evidence/stage-b/b-artifact-007-package-sdk.md)：工具链证据（§6 为 W33-C conformance kit）
- [B-PLAN-002](../evidence/stage-b/b-plan-002-manifest-template.md)：tasks 模板段形状权威
- [B-APPLICATION-002](../evidence/stage-b/b-application-002-update.md)：兼容窗语义（SameMajor/SameMinor）
