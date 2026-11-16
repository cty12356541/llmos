# W82-B2 受控 LLM 五维预算网关库验证

证据 ID `w82-b2-llm-gateway-20261116` · W82-B2 / Attempt 20261116-2 · 2026-11-16 · `H4`

Windows 实施验证及独立审查通过。下列实测为隔离候选的对应结果。主树复验和三平台验证见[汇总门](../../management/stage-e-progress.md#2026-11-16-三服务库验证)。

## 基线与写集

接单时隔离工作树干净，实际 HEAD 为 `cdb0d10d0a68c7d23b04081bc0031942cbb2b893`。该提交将共享 `nlos_capability::service_call` 和依赖预锁一并集成在 `4d097056e3ebd57e130d9303e02e23433fc9d1ba` 上。以下实测来自 `codex/w82-b2-gateway`，HEAD 为 `cdb0d10d`。[补丁回执](artifacts/w82-services-20261116/integration-inputs.json) · [集成源码清单](artifacts/w82-services-20261116/services-source-manifest.json) · [制品摘要](artifacts/w82-services-20261116/sha256-manifest.json)

- `crates/nlos-driver-llm-http/src/{gateway_pricing,gateway_state,gateway_wire,llm_gateway,reservation_journal}.rs` 实现价格表、恢复状态机、冻结 SABI 适配、预算服务与持久日志。
- 同目录 `{http,keypool,lib,shim}.rs` 接通服务和每次 HTTP 尝试的 deadline/授权闸，原 shim 接口保留。
- 同目录 `gateway_fault_tests.rs` 与该 crate 的 `tests/gateway.rs`、`tests/gateway_support/mod.rs` 提供故障注入、真实 SQLite 和环回 TCP 验收。

## 库接口与宿主绑定

`llm_gateway::Gateway` 提供 `llm_chat`、`query_budget`、`attenuate_budget`、`record_external_usage` 和冻结四方法的 `handle_wire`。宿主用 `initialize` 初始化受保护的权威目录，用 `open` 持锁重放现有目录。可信宿主通过 `provision_root` 创建专属预算账户，之后保存并传递已激活的 `GatewayAccountBinding`。衰减须额外提供带真实 recipient 身份、进程代际、session、稳定 child identity 及委托事实的 `ChildSessionBinding`。返回的 `ChildBudgetBinding` 含权威实际账户、父关联、期限和不可变回执，wire 只投影限额和期限。跨主体或应用须提供明确委托并获得绑定实际语义摘要的策略批准。

`GatewayConfig` 注入 shim、`PriceRegistry`、工程上界、可选 `ServiceAuthorizer` 与精确约定的受信取时回调。缺授权器时拒绝；缺价目时拒绝发包。宿主需提供独立认证的 `CallBinding`、有序解析 slot 以及单调同域的 deadline。同一配置的时钟和策略回调同步执行，需避免重入 gateway。方法内部复核 caller、target、method、语义摘要、deadline 与 grant，拒绝 wire Task 或 Reservation 绑定。每次调用、protected replay 和每次本地 provider transport 尝试重新取时授权。尝试 fence 落盘后再取时，后来的撤销能阻止下一次发包和保护结果的返回，已经完成的 provider 响应仍会持久结算。公开的 `AuthorizedServiceCall` 快照无法充当入口凭证。

新增信任面包括 Gateway 与日志状态机、宿主注入的时钟与策略、已激活账户与价目版本、provider 响应的限长解析。硬额度由 ResourceAuthority 守门，受信宿主负责隔离预算写入权和日志目录。恢复同步窄审确认，日志在发布可用对象前无条件同步有效前缀。已有动态同步失败试验在 Prepare 阶段，完整终态的恢复保护经源码追踪。[恢复同步审查](artifacts/w82-services-20261116/b2-recovery-sync-audit.txt)

## 预算、报价与故障语义

- 准入逐维检查权威自用量、各后代已经结算的用量及活跃影子报价。每个账户以 `min(授予额度, 权威当前限额)` 为包络。所有祖先的有效余量均参与准入，descendant 每层祖先只累计一次。查询返回账户子树硬消费、有效剩余和该账户独立咨询用量。
- 三式由同一 journal 写入者串行维护。权威逐账户 `consumed_d <= limit_d`。各边下该子树的权威自用量、后代已结算量和 pending 合计不超过授予限额。发包前各祖先逐维校验未超额，且新 quote 判定通过后已经持久化。结算前用实际量和其余 pending 重验该链。专属域账户的所有 LLM 写入须经本宿主约束。测试模拟外部收紧单账户或祖先配额被观察后拒绝结算。外部重配与本服务之间没有跨 SQLite/journal 原子锁。
- 报价按输入 UTF-8 字节加 8 个预估前导及每条 turn 8 个 framing token，输出使用受信模型的最大值，版本化的 profile 表达 reasoning 在输出帽内或有另计 allowance。速率为每千 token 微信用点，采用 checked 整数和合计向上取整。fixture 中由“汉字”与 U+1F980 组成的请求为 prompt 26、completion 12、total 38、api_calls 1、cost 16。观测值为 prompt 5、visible completion 1、reasoning 3、total 9、api_calls 1、cost 5，使用同一版本。模型不报告必要用量或报告溢出时保留未知故障及可用响应 fingerprint，权威零扣减。
- 日志按 raw account scope、方法及 128 位 key 先比较语义摘要，再使用内部派生值。每个激活账户永久绑定稳定主体、应用与授权域，session 更新不会重置原 key。PREPARED 保存报价、模型/计价档位、精确结算派生值和期限。每次 transport 前持久化 one-use fence。收到已计量响应先追加 SETTLEMENT_INTENT，重验最小祖先包络后持久化可消费标记，再执行精确的 authority consume，最后存 SETTLED/receipt。该标记区分需要重验包络和必须幂等重放已获准消费的恢复情况。
- SETTLE_REJECTED 保留五维实际量、reasoning、价目版本和 provider fingerprint。超限结果拒绝，无截尾。HTTP/解析失败、未知用量、过报价或权威包络、超界结果均没有硬扣减。未知的发包状态经过期限清扫为持久 EffectUnknown 墓碑，原 key 保留；从不以再次调用 provider 恢复丢失响应。实际 transport 次数与逻辑 api_calls 分开，无回执的崩溃窗口记录 durable fence 数和未知 transport 次数。
- 子账户先写 intent，分别幂等调用 SQLite 的 `create_account` 和 `create_llm_token_budget`，最后持久化激活边。过期子账户历史参与其仍有效祖先的硬用量核算。咨询只持久化语义摘要、服务壁钟窗口和不可变用量，省略自由文本标签及客户端原始时间。咨询重复不再追加，同键异摘要冲突，跨会话共享主体限流。
- 保护目录中的 journal 持久结果保存完整成功输出以支持 authorized replay。通常的错误和日志不包含原始 provider 内容、提示词或凭据；服务拒绝含已配置凭据明文的解码结果。故障与对账 API 仅返回类型及受控用量、fingerprint 和回执。
- 配置缺省为 32 个全局/4 个每主体在途调用、每会话和主体 256 咨询新增行/60 s、每父 16 活跃子、全局 4096 活跃绑定、每模型最多 16384 输出 token、每次逻辑调用最多 8 个底层尝试。接受与返回的服务 payload 实际编码均不超过 65536 B，SABI 外层另有 8 KiB 元数据界，HTTP 响应读取上界 512 KiB。日志单条含 framing ≤128 KiB，准入 192 MiB/196608 条，总 256 MiB/262144 条。每次准入预留该调用后继的最坏记录空间。测试用小界验证准入拒绝与跨水位仍能结算。
- 文件锁由操作系统持有。同一权威目录第二 writer 确认被拒绝。启动验证链式双 SHA-256、序号、长度和语义状态转换；合法 torn tail 才会截去，完整损坏拒绝启动；记录成功 ACK 前完成 `File::sync_all`。Unix 创建流程同步父目录。Windows 禁止活跃文件共享删除，支持文件同步，未实现独立的目录项 sync。Windows 初始化须使用宿主预置目录；断电丢失新建目录项时，正常 `open` 拒绝缺失 journal，恢复流程不可把 `initialize` 当作自动补建。

## 首轮实测

Windows x86_64 MSVC，默认 Rust 1.99.0，另以 Rust 1.97 校验最低版本。短目标目录为 `D:/llmos/target/w82-b2` 和 `D:/llmos/target/w82-b2-msrv`。

- `cargo test --locked -p nlos-driver-llm-http` 退出 0，8 库测试、17 gateway 集成测试、9 旧 shim 集成测试、1 旧快照入口编译失败 doctest，共 35 通过。
- `cargo test --locked -p nlos-driver-llm-http --test keypool_and_budget_gate` 退出 0，9/9。
- `cargo clippy --locked -p nlos-driver-llm-http --all-targets -- -D warnings` 退出 0。
- `cargo +1.97 check --locked -p nlos-driver-llm-http --all-targets` 退出 0。
- `cargo fmt -p nlos-driver-llm-http -- --check` 与 `git diff --check` 退出 0。单独执行 `cargo fmt --all -- --check` 在隔离工作树触发 Windows 长命令行错误 206，退出 1。主工作区全仓格式校验留给集成。额外的 `+1.97 clippy` 探测在未修改的 `nlos-store` 既有 doc lint 停止，默认工具链门已完成。
- 分别注入 PREPARED、DISPATCHED、SETTLEMENT_INTENT、可消费标记、authority consume 已完成及 SETTLED 已落盘故障。另注入子账户 intent、账户事务、预算事务三个位置，模拟部分写与同步失败、篡改序号/长度/中段/尾段、重复记录、启动读界以及 writer 互斥。重开恢复使用真实 journal 和 SQLite，测试故障为进程内断点模拟关闭与重开。
- 环回 TCP 阻塞点验证了 parent/兄弟/孙交错、并发限制、各维不足零连接、发包前撤销零连接、发包后撤销保留扣减、timeout/断流/解析/缺用量、429 failover 两尝试一逻辑调用、价格上界拒绝、外部缩限拒结和实际 65536/65537 B 编码结果。真实网络负例使用受控本机 provider；报告中的账户与凭据为合成夹具。

提供的 D8 能力是 OBSERVED 影子额度组合。单账户硬限额以 ResourceAuthority 为守门；failover 或无法观察的尝试可以增加外部真实成本。真实 provider H5、外部旁路治理、真实断电、Linux/macOS 实跑、daemon 的身份/slot/child session/revocation/Operation 接线、SDK 和外部消费者均未在本 lane 验证。候选没有暂存、提交、推送或 CI 晋升。

## 审查修复 1

独立审查指出并发采样与发布分离会把正常递增的时钟误判回退。使用独立时钟互斥锁把回调采样、墙钟与单调时钟比较、成对发布线性化。真实任一域回退及回调失败都拒绝，失败的采样不会移动任一时钟高水位。该锁与 Store 分离，保留原授权和 deadline 检查。

- `cargo test --locked -p nlos-driver-llm-http --test gateway clock` 修复前退出 101，正常并发采样和失败采样的成对发布两条新测试返回意外 `TimeUnavailable`。修复后同一命令退出 0，3/3，含真实回退、时钟回调失败、期限及权限拒绝零 TCP 的断言。
- `cargo test --locked -p nlos-driver-llm-http` 修复后退出 0，8 库测试、19 gateway 集成、9 旧 shim 集成、1 compile-fail doctest，共 37 项通过。
- 修复后默认 Clippy、Rust 1.97 all-targets 编译、crate fmt 与 Git diff 检查退出 0。本轮修复未重跑隔离工作树全仓格式和跨平台检查。增量独立复审通过。[并发取时修复前](artifacts/w82-services-20261116/b2-clock-red.txt) · [修复后](artifacts/w82-services-20261116/b2-clock-green.txt) · [祖先预算反例](artifacts/w82-services-20261116/b2-ancestor-red.txt) · [包测试](artifacts/w82-services-20261116/b2-library-suite.txt) · [独立复审](artifacts/w82-services-20261116/b2-review.txt)
