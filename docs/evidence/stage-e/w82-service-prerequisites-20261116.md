# W82-B 服务库前置验证

- 证据 ID 为 `w82-service-prerequisites-20261116`。任务 `W82-SERVICES-PREREQUISITES-DOC / 20261116-2`，2026-11-16。保障等级限于本页的库合同与依赖验证（`H4`）。
- 候选以 `4d097056e3ebd57e130d9303e02e23433fc9d1ba` 为共同基线。共享调用合同和依赖预锁通过独立规格、质量审查，依赖预锁另通过安全审查。两个受审候选合流后通过主树定向复验，集成版本的远端三平台 CI 仍待执行。
- 设计与派发分别归[实施收口](../../management/w82-b-design-draft.md#15-2026-11-16-第二波服务库实施收口)及[前置工作项](../../management/nlos-goal-delivery-plan.md#w82-b-service-prerequisites服务库公共前置)。

## 共享调用合同

`nlos_capability::service_call` 接收由可信宿主提供的调用身份、目标、服务重算摘要和已解析的精确能力句柄。它先检查方法所需幂等键、同域单调截止时间及 1–64 个不重复句柄，再调用注入的 `ServiceAuthorizer`。只有答复与完整调用及动作一致、授予句柄包含在原列表中且当前墙钟位于授予闭区间，才构造私有只读结果。默认授权器拒绝，错误保留类型且不拼接请求数据。证据中的正例使用受控策略替身，实际身份、权利、祖先、用途和会话映射由 INT 配置真实策略并验收。

Windows、Rust/Cargo 1.99.0 上，目标测试在模块缺失时以 E0432 失败，实施后 12/12 通过。另通过 capability 全包的 47 项单元及集成测试和 1 项编译失败文档测试，以及全 target Clippy（`-D warnings`）。长路径工作树的全 workspace fmt 命令触发 Windows error 206，45 个 workspace package 以五批执行同等格式检查通过。[缺模块失败](artifacts/w82-service-prerequisites-20261116/context-red.txt) · [目标测试](artifacts/w82-service-prerequisites-20261116/context-contract.txt) · [完整包测试](artifacts/w82-service-prerequisites-20261116/context-capability.txt) · [Clippy](artifacts/w82-service-prerequisites-20261116/context-clippy.txt) · [分批格式](artifacts/w82-service-prerequisites-20261116/context-fmt-batches.txt)

## 依赖预锁

候选将 `cap-std` 与 `cap-fs-ext` 精确预锁为 `4.0.3`，六份根及独立工作区锁均解析 `cap-primitives 4.0.3`。B2/B3 增加对 capability 的单向消费，B3/B4 接入现有 schema codec 所需的清单依赖。六份锁在 Windows Rust/Cargo 1.97.1 上各通过 `cargo metadata --locked --offline` 与 `cargo tree --locked --offline --target all`。三个消费 crate 的 `cargo +1.97 check --locked` 通过，sample-app 与 ROAD-E-001 的独立清单也通过定向编译检查。既有锁供应链测试 3/3，六份锁的 registry 来源与校验和格式审计无违规。校验过 16 个冻结制品文件的字节摘要，均与候选变更前一致。[解析状态](artifacts/w82-service-prerequisites-20261116/dependency-resolution.txt) · [三目标 MSRV 编译](artifacts/w82-service-prerequisites-20261116/dependency-msrv-check.txt) · [供应链测试](artifacts/w82-service-prerequisites-20261116/lockfile-conformance.txt) · [锁差异](artifacts/w82-service-prerequisites-20261116/lock-audit.json) · [冻结制品](artifacts/w82-service-prerequisites-20261116/sealed-product-sha256.json)

以同一工具链在原样基线复现 sample-app 的 `--locked` 解析失败。基线已经新增的路径依赖与版本下限须先补录到其独立锁。对照可解析的基线后，W82 预锁本身无既有包升级或删除；sample-app 的基线修复与 W82 新增项在锁差异中分别记录。[原样基线失败日志](artifacts/w82-service-prerequisites-20261116/sample-app-baseline-locked.txt)

`4.0.3` 为所选 cap 系列安全修复版本。此处验证依赖解析与编译，具体 rooted opener、首次打开的竞态验证、三平台文件系统约束以及第三方 Windows 端点环境兼容性由 B4 另行取证。[安全公告](https://rustsec.org/advisories/RUSTSEC-2026-0482.html) · [D5 实施收口](../../management/w82-b-design-draft.md#15-2026-11-16-第二波服务库实施收口)

## 主树复验

短路径 Windows 主树实际运行以下命令，均退出 0。该次编译包含共享调用合同及全部预锁变化。候选补丁摘要和命令的退出码、输出字节数见[执行回执](artifacts/w82-service-prerequisites-20261116/integrated-command-receipt.json)。

- `cargo test --locked -p nlos-capability` 通过 47 项单元及集成测试（含 12 项共享合同）和 1 项编译失败文档测试。[原始输出](artifacts/w82-service-prerequisites-20261116/integrated-capability-test.txt)
- `cargo clippy --locked -p nlos-capability --all-targets -- -D warnings` 与 `cargo fmt --all -- --check` 均通过。该次全 fmt 无输出。[Clippy 输出](artifacts/w82-service-prerequisites-20261116/integrated-capability-clippy.txt)
- `cargo +1.97 check --locked -p nlos-capability -p nlos-driver-llm-http -p nlos-wasm-sandbox` 通过。[原始输出](artifacts/w82-service-prerequisites-20261116/integrated-three-target-msrv-check.txt)
- `python -B -m unittest discover -s tests/conformance/schema -p test_lockfile_supply_chain_gate.py` 通过 3/3。[原始输出](artifacts/w82-service-prerequisites-20261116/integrated-supply-chain.txt)

## 证据范围

本轮为 B2/B3/B4 消费的共享调用合同与锁文件前置。真实策略与认证证明、受控网关/工具/Blob 服务、daemon 接线、SDK 与外部消费者都另有验收门。新制品转存为 UTF-8/LF，去除捕获文本末尾空行。清单同时保存本地原始捕获和 Git 所存文本的长度与摘要。历史 B 的证据与归档继续绑定 `bec56f99` 的源快照。原 81 路径中的当前设计对象本轮获得前向追加授权，其余 80 路径字节保持，旧清单摘要继续指向原版。[新制品摘要](artifacts/w82-service-prerequisites-20261116/sha256-manifest.json) · [B 契约历史证据](w82-b-contract-20261116.md)
