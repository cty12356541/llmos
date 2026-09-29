# B-PLAN-002：package manifest `tasks` 模板段（TaskPlan 声明面 template face）

> 状态：`PARTIAL_PASS`（**模板面**——manifest 扩段 + 验签 golden + 模板→proposal 编译，2026-09-20，W28-B）
>
> 对应：[ADR-0016 决定 1](../../management/adrs/0016-task-plan-declaration-surface.md)（manifest 扩 tasks 模板段，additive golden 纪律）；[议题 35 §6](../../discussions/35-TaskPlan声明面设计.md) 验收门 G6 与 `[PLAN-OVERRIDE-001]`；[进度单 §6.5.3](../../management/stage-b-progress.md) W28-B 车道行；[B-PLAN-001](b-plan-001-declaration-surface.md)（状态面，W28-A）
>
> 实现：`crates/nlos-artifact/src/package.rs`（模板段模型 + templated message + 共享 shape validator + verify 第二入口）；`crates/nlos-application/src/task_templates.rs`（模板→TaskPlan proposal 编译，proposal 数据、不开 plan store）
>
> 范围纪律：本 lane 只落**模板面**。安装/启动时把编译产物接进 `apply_plan_revision`（Slice K 后续纵切段）、TaskSpec 关联字段（W29-A）、Dependency Resolver（W29-B）、物化门接线（W31-A）均不在本 evidence 声明范围。

## 1. 本切片目标

把「TaskPlan 声明从哪来」接进 signed Package：package manifest additive 扩 `tasks` 模板段（声明式 node/依赖/资源上界模板，随包签名不可变），安装/启动时实例化为与状态面（nlos-plan，W28-A）**同一 schema** 的 TaskPlan proposal。三道门：G6 旧签名包仍可验装（负路径硬门）、新段 additive golden、`[PLAN-OVERRIDE-001]` 编译等价（模板不得自成第二声明方言）。

## 2. 实现事实

### 2.1 为什么是平行签名面而不是 `PackageManifest` 加字段

additive 的第一选择本是在 `PackageManifest` 上加 `tasks: Option<…>` 字段（absent 时 framing 逐字节不变）。但 `crates/nlos-slice-k/src/package.rs:120` 以**穷举 struct literal** 构造 `PackageManifest`/`SignedPackage`（生产代码），而 nlos-slice-k 不在本 lane 写集；rustc 1.97.1 上 default field values（RFC 3628）仍未稳定，任何新必填字段都会击穿 slice-k 编译。故落为**平行 additive 面**：

- `SignedPackageWithTasks { manifest: PackageManifest, tasks: Vec<PackageTaskTemplate>, signer, signature }`——刻意扁平、不内嵌 `SignedPackage`：内嵌会携带第二枚 legacy 签名，其单独验签即静默丢段（降级面）。
- `package_manifest_with_tasks_message(manifest, tasks) = SHA256("llmos/artifact/package-manifest-with-tasks/v1" ‖ package_manifest_message(manifest) ‖ 段framing)`——段 framing 为 `u64 BE 计数 + 每模板 node_key/kind 字节/binding_digest/u64 BE 依赖计数+依赖键(声明序)/四个 body digest`，链式复用已规范的 legacy 摘要（base framing 只存在一份）。
- 两域分隔 ⇒ templated 摘要与 legacy 摘要不可能碰撞：**剥段（templated 签名配 legacy 面）与注段（legacy 签名配 templated 面）都是 `PackageSignatureInvalid`**（负路径测试 `verify_with_tasks_fails_closed_on_segment_tamper_and_strip` 双向证伪）。
- legacy `verify_package` 与 `PackageManifest`/`SignedPackage` 类型**零改动**（仅方法体抽为共享私有核 `verify_signed_package`，逐语句保持原序：shape→digest→lock→replay→identity 验签→`BEGIN IMMEDIATE` 内 binding+receipt）；G6 由结构保证 + 测试显式复证。

### 2.2 模板段模型（manifest face，声明字段=状态面字段）

`PackageTaskTemplate { node_key:[u8;16], kind:PackageTaskKind, binding_digest:[u8;32], dependency_keys:Vec<[u8;16]>, input_selectors_digest/output_contract_digest/policy_digest/resource_ceiling_digest:[u8;32] }`——与 `nlos_plan::PlanNodeDeclaration` 字段一一对应（node_key 即声明本地稳定身份）。`PackageTaskKind {AgentRole=1, Executable=2}` 镜像 `PlanNodeKind` 的两值面与线编码，但 manifest 自持枚举（签名包 schema 不随 plan authority 类型演进漂移），编译侧全映射 match 保证不漂。

共享 shape 权威 `validate_task_templates`（pub）：非空、`MAX_TASK_TEMPLATES_PER_MANIFEST=100_000` / `MAX_TASK_DEPENDENCIES_PER_TEMPLATE=256`（镜像 plan 侧 admission bound，各 crate 自持常量）、node_key 唯一、无自依赖、依赖必须解析到段内已声明 key。**环检测刻意留给 plan authority**（`[PLAN-DAG-001]` 单一 owner：manifest 面只保证引用局部性，编译产物与直接声明一样过 plan 自己的 admission）——该 validator 同时被 `verify_package_with_tasks` 与 `nlos-application` 编译器调用，两面共享同一 shape 权威、零方言漂移。

### 2.3 验签与安装接线

`ArtifactStore::verify_package_with_tasks`：legacy shape + 段 shape → templated 摘要 → 共享核（replay 按 (manifest digest, signer, signature)、identity 验签、entry content binding、immutable receipt 同表同形落库；receipt 的 `manifest_digest` 即 templated 摘要，entry_count 仍为内容绑定条目数）。**无 schema 迁移**：旧表、旧行、旧 receipt 解码不变；`install_application` 零改动即消费 templated receipt（测试 `templated_package_installs_through_the_same_receipt_path`）。

### 2.4 模板→proposal 编译（`nlos-application::compile_task_templates`，`[PLAN-OVERRIDE-001]`）

`compile_task_templates(&SignedPackageWithTasks, idempotency_key, applied_at_ms) -> Result<nlos_plan::ApplyPlanRevisionRequest, TaskTemplateError>`：`plan_id: None`（初始 revision，plan authority 幂等键派生 PlanId）、逐模板字段全映射、依赖序逐位保留、kind 全映射 match（plan 侧加变体即编译失败，漂移=构建失败）。先过共享 `validate_task_templates`（已验签段不可能失败，fail-closed 于未验输入）；**不重验签名、不开 plan store、不落任何 durable 状态**——纯 proposal 数据。依赖注记：为此给 `crates/nlos-application/Cargo.toml` 加 `nlos-plan` path 依赖（**read-only 消费其 pub 声明类型**；nlos-plan 源码与根 Cargo.toml 零改动，Cargo.lock 仅工具链生成的这一条边——任务书「normal dependency if already declared」条款的解释见 §5）。

### 2.5 TDD 红→绿记录

测试先行：`tests/package_task_templates.rs`、`tests/manifest_task_templates.rs`、support 助手先落，首跑红（新类型未实现，E0432/E0599 编译失败）→ 实现 package.rs 模型/validator/双入口与 task_templates.rs 编译器 → 绿；期间 clippy -D warnings 逼出 `from_ref` 代替 `&[x.clone()]`、`doc_markdown` 反引号、core 传参改引用，均按 lint 修正而非 allow。

## 3. 验证证据

新增测试 15 个：nlos-artifact lib 单测 2（`templated_message_framing_is_canonical`：链接性/域分离/每字段参与/依赖序与模板序参与；`task_template_shape_validation_matches_the_declared_rules`）+ 集成 4（valid 验签+receipt 回读、跨重启幂等 replay 逐字节相等+同键异形 `IdempotencyConflict`、剥段/注段/改模板三向 tamper 全 `PackageSignatureInvalid` 且零 durable 行、六类 malformed 段 typed `PackageManifestInvalid`）+ nlos-application 集成 4（**G6**、templated 安装、**PLAN-OVERRIDE-001 编译等价**、malformed 段 fail-closed）+ 两 crate support 助手。既有 golden 零改动零回归：`package_signature.rs` 7 用例、`manifest_message_framing_is_canonical`、application_authority 43 + fault 7 全绿原样。

**G6**（`g6_old_shape_package_verifies_and_installs_byte_identically`）：无段旧包在模板面同 build 下经 legacy 面验签（receipt 摘要 == `package_manifest_message` 逐字节）→ 安装 → 同键 replay 逐字节相等、代际不双跳。

**PLAN-OVERRIDE-001**（`plan_override_001_templated_segment_compiles_to_the_direct_plan_proposal`）：三模板段（含双依赖与两 kind）编译产物与手工直写的 `ApplyPlanRevisionRequest` **结构相等（逐字节）** 且 canonical 序列化摘要相等；依赖重排即不同 proposal（序是声明数据，与 plan 面一致）；同输入重编译确定。

本地验证命令与结果（2026-09-20，final fmt 后）：

```text
cargo test -p nlos-artifact                                 # PASS：72 passed / 0 failed（含新增 6）
cargo test -p nlos-application                              # PASS：60 passed / 0 failed（含新增 4）
cargo clippy -p nlos-artifact -p nlos-application --all-targets -- -D warnings   # PASS：exit 0
cargo +nightly-2026-08-01 clippy -p nlos-artifact -p nlos-application --all-targets -- -D warnings  # PASS：exit 0
cargo fmt -p nlos-artifact -p nlos-application -- --check   # PASS（stable 与 nightly-2026-08-01 双通过）
cargo check -p nlos-slice-k                                 # PASS：平行面零跨界破坏（slice-k 未触碰仍编译）
```

## 4. 已知限制与 deferred minors（如实登记）

- **「normal dependency if already declared」解释**：任务书预想 nlos-plan 类型可能已声明为依赖；实际本 wave 前无任何 crate 依赖 nlos-plan。按 ADR-0016 决定 1「模板编译为与 B 相同的 TaskPlan/TaskNode schema」的硬性要求，在**本 lane 写集内**的 `crates/nlos-application/Cargo.toml` 加 path 依赖（read-only、无环、不动 nlos-plan/根 Cargo.toml），而非 STOP 丢掉验收门 PLAN-OVERRIDE-001——如编排方不认可此解释，回退只需删该依赖行与 task_templates 模块。
- evidence 索引（`docs/management/evidence-index` 系）未收录本文件：不在本 lane 写集，按 W28-A 先例（6c7a404 由 docs 车道单独收录）留给索引车道。
- `SignedPackageWithTasks` 的跨进程序列化/Envelope 通道不在本切片（与 legacy 面同边界）；模板 body（selector/contract/policy/ceiling）的**解析模型**仍是 digest-bound 不透明段（nlos-plan model.rs 注记 W28-B/W29 territory——本 lane 只落 W28-B 声明面，解析模型留 W29+）。
- 未做：同 signer 双面签名共存语义的专门 golden（两面各一枚签名是两枚独立签名对象，语义已在域分离下自然成立）；100_001 模板上界用例走 shape 短路（不触发 Ed25519）以控测试时长。

## 5. 未运行项（显式列出）

未运行 `cargo test --workspace`（任务边界禁）、三平台 CI/MSRV/Pages（波次屏障统一跑）、真实断电矩阵（本切片无新 durable 协议——无 schema 迁移、无新表、无新 commit 路径，共享核即 legacy 核）。
