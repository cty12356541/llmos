# W82-B 车道设计定稿草案:SABI v1.1 薄通道 + L-015 大载荷原语(供维护者终裁)

> **性质:未跟踪的工作文档。若采纳为正式设计,需按 AGENTS.md 规则 CRUD 进项目知识体系,本文件不替代该流程。**
>
> **维护者终裁(2026-11-08,原话「D1-D11 全部按建议裁定(含第三通道 BlobReference,9→12)」):各章「请裁定」均按建议采纳——D5 第三通道 `nlos.sabi.BlobReference` 获裁,冻结通道 9→12;D10 写集扩展归属按 §10.3 建议执行。本文档自此为裁定后定稿,登记动作归 W82-INT。**
>
> 日期:2026-11-08 · 基线:main `72c75e596` · 通道:W82-B-SABI-V11-THIN-WAIST
> 权威依据:[stage-e-progress.md](stage-e-progress.md) §E.6.1.8、[E-DOGFOOD-001](../evidence/stage-e/e-dogfood-001-qq-agent-l015-channel-payload-bound.md)(L-015 裁定:选项 B 默认优先、选项 C 否决、选项 A 须附等强 TCB/DoS 论证)、ADR-0010/0014/0022/0027
> 纪律:本文档为设计定稿,**零代码、零 claims.yaml 预支**(AGENTS.md 第 7 条);所有 file:line 锚点已于 2026-11-08 在 main `72c75e596` 全量核验(9 路并行勘察,346 次工具调用)。

---

## 0. 决策点速览(每项一行;正文各章为「建议 X / 理由 Y / 代价 Z / 请裁定」全文)

| # | 议题 | 建议 | 请裁定 |
|---|---|---|---|
| D1 | tool_sandbox 方法数 2 vs 3 | **2 个**(list_tools/invoke_tool);取消走 OperationControl,等待走 WaitControl,与规格「协同」措辞一致 | 修正规格「~3 个」口径 |
| D2 | 通道命名与载荷上界 | `nlos.sabi.LlmGateway`/`nlos.sabi.ToolSandbox`(无 Control 后缀);`MAX_LLM_GATEWAY_PAYLOAD_BYTES = MAX_TOOL_SANDBOX_PAYLOAD_BYTES = 64 KiB`(若 D5 第三通道获裁,另加 `MAX_BLOB_REFERENCE_PAYLOAD_BYTES = 64 KiB`) | 采纳取值 |
| D3 | 超限失败即关闭语义 | **逐调用拒帧 + 每会话拒帧预算**(窗口内超限/畸形帧超 N 次升级为断连,混合 (a)+(b));不吊销预算;「拒帧后连接存活」为 W82-A 新语义,列为 A/INT 显式验收项 | 采纳并钉入 proto 语义注释 |
| D4 | record_external_usage 信任模型 | **纯咨询入账**(仅对账,不参与硬预算扣减);复用 128-bit 幂等键去重 + 单调追加 | 采纳 |
| D5 | L-015 落点 | **确认选项 B**;原语落独立第三通道 `nlos.sabi.BlobReference`(推荐)或寄宿 tool_sandbox(回退);能力权威**另立引用令牌表**(additive migrate_v3,不动 heads);线上 slot↔16B id 映射按 §5.4 | 终裁 B + 第三通道取舍 |
| D6 | current_epoch 与 epochs/v1.1 | **维持 current_epoch: 1**(新通道 introduced_in_epoch: 1);**建 epochs/v1.1/** 黄金目录(最小向量集) | 采纳口径 |
| D7 | LLMOS_ROOT_CAPABILITY_SLOT 值格式 | **32 位小写十六进制 = 16 字节 CapabilityId**(`^[0-9a-f]{32}$`),不含 slot 号、不含 generation | 钉死供 W82-C 对表 |
| D8 | 五维预扣组合 | nlos-driver-llm-http 内:**门内影子预留账本**(journal 文件)+ 权威 consume 为唯一持久扣减;失败零扣减;服务端价目表;父≥子不变量测试 | 采纳组合方案 |
| D9 | 工具 ABI 与执行模型 | per-call 宿主进程(现有沙箱零协议改动);取消=杀子进程→EffectUnknown/CancelledBeforeEffect 显式建模;幂等走持久注册表(不照搬内存 PendingCallFence);常驻宿主延后并给实测计划 | 采纳 v1.1 范围 |
| D10 | 机械面 | 9→**12**(推荐,含第三通道)或 9→11(回退);全部落点、两套写集处置与四语言 SDK 集合断言松绑见 §10 | 裁定通道数与写集扩展 |
| D11 | 落地顺序 | 机械 schema 步 → llm_gateway 服务层 → tool_sandbox 服务层 → L-015 原语,各自验收门见 §11 | 采纳顺序 |

**给维护者的两个显式裁定请求**(超出常规决策点):其一,D5 推荐增设第三通道 `nlos.sabi.BlobReference`,使冻结通道总数 9→12 而非立项简报暗含的 9→11(§5.2 论证为什么两个通道装不下 L-015);其二,D10 有三组文件不在任何 W82 车道声明写集内,需按 AGENTS.md 显式扩写集(§10.3)。

---

## 1. D1:tool_sandbox 方法数——建议 2,修正规格「~3 个」口径

**选项**:(a) 2 方法 list_tools + invoke_tool;(b) 3 方法,第三方法候选为「完成/取消观察点」或「工具目录订阅」。

**建议 (a) 2 个。理由**:
- 取消与观察是 SABI 公共语义而非通道私有语义:`SABI-COMMON-001`/`SABI-RECEIPT-001` 下,异步 `Operation`、`cancel`、`query`、幂等由 `OperationControl`(已冻结 v1.0,`CancelOperationRequest`/`QueryOperationRequest` 消息已在 [operation_control.proto](../../schema/nlos/sabi/v1/operation_control.proto) 注册)与 `WaitControl` 承载。W82-A 正是把这些已冻结通道挂入 daemon([wait_control](../../crates/nlos-wait-control/src/lib.rs:79) 已实现完整 crate,只差挂载)。规格 §E.6.1.8 自己的措辞是「`list_tools`、`invoke_tool` **并协同 OperationControl / WaitControl**」——「协同」即第三个"方法"实为跨通道协作,不是新 RPC。
- 在 tool_sandbox 上另设取消/观察方法会与 OperationControl 形成双真相源,且每个新增方法都是永久兼容面(v1.1 冻结后只能 additive 演进)。
- 工具目录订阅(服务端推送)需要 SABI 尚不存在的推送语义(当前唯一传输是 `LocalRpcService.Exchange` 一元交换,[envelope.proto:138-140](../../schema/nlos/sabi/v1/envelope.proto));目录变化仅发生在包安装/卸载时,`list_tools` 幂等重拉的代价可忽略。YAGNI:订阅留作未来 v1.2 additive 方法。

**代价**:规格 §E.6.1.8 的「~3 个 RPC 方法」口径需修正为 2 个(合计方法数 4+2=6,另加 L-015 原语 3 个,见 D5);该修正属 docs/management 写集,由 W82-INT 登记执行,本文档仅建议。

**请裁定**:确认 tool_sandbox = 2 方法;规格口径修正授权 INT。

## 2. D2:通道命名与 MAX_*_PAYLOAD_BYTES 取值

### 2.1 命名(无 Control 后缀)

既有 9 通道命名语法:`<领域><角色>`,Control 后缀专属宿主控制面(SystemControl/OperationControl/TakeoverControl/WaitControl/ChannelControl);能力作用域的服务暴露面不带后缀(ServiceDirectory/PrincipalHandshake/DriverProvider)。两个新通道是**能力作用域的服务暴露面**(按 CapabilityHandle 委托调用),不是宿主控制面:

- `nlos.sabi.LlmGateway`(文件 [schema/nlos/sabi/v1/llm_gateway.proto](../../schema/nlos/sabi/v1);service 分派名 `"llm_gateway"`;方法字符串 `llm_chat`/`query_budget`/`attenuate_budget`/`record_external_usage`)
- `nlos.sabi.ToolSandbox`(文件 tool_sandbox.proto;service `"tool_sandbox"`;方法 `list_tools`/`invoke_tool`)
- (D5 推荐)`nlos.sabi.BlobReference`(文件 blob_reference.proto;service `"blob_reference"`;方法 `mint_reference`/`read_reference`/`release_reference`)

与 §E.6.1.8 DAG 表proto 文件名、W82-D 客户端名(`LlmGatewayClient`/`ToolSandboxClient`)及 SDK 表登记名一致。加 Control 后缀会破坏命名语法并把服务面误标为控制面——不建议。

### 2.2 载荷上界:一律 64 KiB

先例([crates/nlos-schema/src/lib.rs:50-58](../../crates/nlos-schema/src/lib.rs)):应用面六个 64 KiB 通道(ServiceDirectory/OperationControl/SystemControl/TakeoverControl/WaitControl/ChannelControl——ChannelControl 即 L-015 顶到的那个上界);PRINCIPAL_HANDSHAKE 4 KiB(定长握手)、DRIVER_PROVIDER 16 KiB(驱动帧)、Envelope 外层 1 MiB。

**建议**:`MAX_LLM_GATEWAY_PAYLOAD_BYTES = MAX_TOOL_SANDBOX_PAYLOAD_BYTES = MAX_BLOB_REFERENCE_PAYLOAD_BYTES = 64 * 1024`,新增于 lib.rs:58 之后,按既有 per-call-site 模式接入(仓库无按名查限值表,每个 encode/decode 函数硬编码自身通道常量,见 [lib.rs:2001-2006](../../crates/nlos-schema/src/lib.rs) ChannelControl 范式)。

**内存/DoS 有界性论证**:
1. **解码边界**:`decode_bounded_with_limit` 在 `M::decode` 之前先做长度检查([lib.rs:3070-3081](../../crates/nlos-schema/src/lib.rs)),超限帧零分配拒绝;`encode_bounded_with_limit` 对结果再守一边([lib.rs:3052-3064](../../crates/nlos-schema/src/lib.rs))。响应方向同界:任何服务层结果超过 64 KiB(如超大工具目录)必须以 typed 错误拒绝而非放大——无放大面。
2. **并发积压有界**:W82-A 的 L-008 循环是单连接有界多轮 exchange(逐轮处理、空闲超时 fail-closed),每连接在途回 envelope 外层 ≤ 1 MiB(MAX_ENVELOPE_BYTES),服务载荷 ≤ 64 KiB。最坏内存 = 连接数 × 在途数 × 1 MiB,与 v1.0 现状同包络——新通道不恶化任何一维。
3. **访问前置**:两通道(及 BlobReference)分派前均经 principal 握手 + capability 校验(CAP-RIGHTS),匿名面零新增。
4. **64 KiB 的功能充裕性**(逐方法核过):llm_chat 请求含会话历史——64 KiB ≈ 1.6-2.1 万 CJK 字符或 ~1.6 万 token 级别的提示,覆盖 qq-agent 现网流量;更长上下文属大载荷,**正是 L-015 引用原语的管辖域**(帧传引用不传字节),不是上调帧界的理由——这是 D5 与 D2 的设计互锁:帧界不动,大载荷走引用。非流式补全回复同理受 64 KiB 约束(超出即 typed 拒绝;流式方法留待未来 additive)。

**代价**:>64 KiB 的 llm_chat 上下文在 v1.1 被 fail-closed 拒绝(诚实边界,回执给客户端明确错误与建议);voice 等富媒体本来就不走 llm_chat。

**请裁定**:采纳命名与三个 64 KiB。

## 3. D3:「超限失败即关闭」语义——建议逐调用拒帧 + 每会话拒帧预算(混合 (a)+(b))

**选项**:(a) 拒帧(逐调用 typed fail-closed,连接保持);(b) 关闭整条 SABI 连接;(c) 吊销该会话的预算授予(能力令牌级)。

**建议:(a) 为基线,叠加「每会话拒帧预算」超限升级为断连(即滥用态下自动升为 (b))。理由**:
- **语义一致的诚实口径**:通道层的拒帧语义与 9 个冻结通道一致——传输层 `FramedIo` 在 4 字节长度前缀处即 `Err(FrameTooLarge)`、不读帧体([crates/nlos-ipc/src/lib.rs:358-365](../../crates/nlos-ipc/src/lib.rs)),服务层编解码以 `CompatibilityError::FrameTooLarge` 拒绝([lib.rs:3052-3081](../../crates/nlos-schema/src/lib.rs))。但**连接层行为今天事实是「一条连接恰好一次 exchange,帧错误即弃连」**([daemon.rs:1193-1194](../../crates/nlos-system-control/src/daemon.rs)、:1284)——「拒帧后连接存活」本身是 W82-A 循环将引入的新语义。因此 (a) 与 (b) **同样依赖** W82-A 的错误处理设计,不存在「(a) 无依赖、(b) 有依赖」的不对称;「catch 拒帧后继续循环」必须列为 W82-A/INT 的显式验收项(§13.1)。
- **放大向量的诚实修正**:前缀即拒意味着恶意客户端**4 字节**即可在 keep-alive 循环下触发一次完整 typed 响应(~30-100x 字节放大 + daemon CPU)——「攻击者须发送 ≥64 KiB」只对「合法 envelope 内的通道级超限载荷」成立。故纯 (a) 不够:叠加**每会话拒帧预算**(建议默认:60s 滑动窗口内拒帧+畸形帧累计 > 8 次 ⇒ 断连并计遥测)。攻击者反复滥用即失去已认证会话(重握手成本归攻击者),放大被预算封顶。
- (b) 单独作基线仍是**跨通道连坐**:W82-A 会话复用后一条长连接同时承载 wait_control 等待与 llm_chat,一个超限帧即撕断无辜通道的在途状态;预算机制只对**累计滥用**断连,单个偶然超限(客户端 bug)不连坐——这是混合方案优于纯 (b) 的点。
- (c) 混淆了计量与成帧:载荷超限不是预算欺诈,以吊销预算回应是量罚失当;且为一条新语义引入能力权威联动,扩大 TCB 交互面,违背最小变更。

**落地**:钉进 llm_gateway.proto / tool_sandbox.proto / blob_reference.proto 语义注释,建议注释文本(英文,与仓库 proto 注释惯例一致):
```
// A frame whose encoded payload exceeds MAX_*_PAYLOAD_BYTES (64 KiB) is
// rejected with a typed FrameTooLarge failure for that call only. A
// per-session budget bounds rejected/malformed frames (default: more
// than 8 within a 60s sliding window tears the connection down).
// Oversized payloads MUST travel by reference (BlobReference), never by
// enlarging this bound.
```

**代价**:新增两个可配常量(窗口与阈值)与断连遥测;拒帧-存活语义的端到端验收推迟到 W82-A/INT(与 (b) 对称,如实声明)。如维护者坚持纯 (b)(零容忍断连),代价是无辜通道连坐——不建议。

**请裁定**:采纳混合方案(逐调用拒帧 + 每会话拒帧预算),注释文本如上钉入。

## 4. D4:record_external_usage 信任模型——建议纯咨询入账

**选项**:(a) 纯咨询(仅对账可见,不参与硬扣);(b) 参与硬预算扣减(配任务资源族 quote/upper_bound+usage 单调校验上界防低报)。

**建议 (a)。理由**:
- **执法输入必须服务端派生**(绝对安全宗旨):外部用量是客户端自报数字——供给方响应从未经过我们的线。若让它参与硬扣,恶意客户端低报即可让真实开销越过预算上限,「五维预算 = 内核级硬资源」的承诺被自报数字击穿;现有 `cost_microcredits` 调用方自报(实际在 [shim.rs:258-262](../../crates/nlos-driver-llm-http/src/shim.rs) 的 `with_cost_microcredits`,生产调用方全部默认 0,唯一调用是单测 `tests/keypool_and_budget_gate.rs:558`)已经是这个教训的微缩版。任务资源族的单调校验([nlos-resource/src/lib.rs:1489-1503](../../crates/nlos-resource/src/lib.rs) `UsageNotMonotonic`/`UsageExceedsUpperBound`)防的是**高报**(消耗超过预留上界),防不了低报——低报在纯自报模型下无上界可锚。
- **咨询数据派生不出任何权利**:不入账硬预算 → 低报只歪曲自己的仪表盘,高报也换不来任何额度。信任面归零。
- 硬熔断承诺不被削弱的**诚实口径**:TCB-EGRESS-001 实为 `PARTIAL_PASS` 且边界有限——Wasm 强沙箱内零 socket 导入(实例化即拒);受控原生组件是**上架认证时的清单声明检查**(certify Gate 2),宿主 OS 防火墙级流量过滤明确不在该层范围内;且外部 SABI 客户端进程(qq-agent 等)本身是未沙箱的普通 OS 进程、持有自己的网络能力。真实残面 = Z4 批准的原生组件 + 自带供给方密钥的未沙箱客户端进程;这些路径的用量本就只能事后对账。**proto 语义注释须写明:五维硬上限只约束经 llm_gateway 路由的开销**。v1.2 若引入「供给方签名回执」可再评估硬扣(留 additive 演进缝)。

**设计要点**(供服务层实现):咨询记录追加进 D8 的影子账本 journal,标记 `source = external_advisory`;以 128-bit 幂等键去重(同键同摘要重放、异摘要冲突,镜像 [nlos-store](../../crates/nlos-store/src/lib.rs) `begin_idempotent_operation` 的 digest 语义);`query_budget` 响应中单列 `external_advisory_*` 字段(绝不并入 remaining 计算);单调追加、只增不减。

**代价**:外部路径用量不触发硬熔断(诚实边界,写入 proto 注释与验收文档);对账数据依赖客户端诚实。

**请裁定**:采纳 (a)。

## 5. D5:L-015 选项 B 终裁确认、原语落点、能力权威演进与 slot 映射

### 5.1 终裁确认:维持选项 B(裁定登记的默认优先路线)

第 12 章给出选项 B 的 TCB 增量分析与内存/DoS 有界性论证,以及选项 A 的**等强对照**(裁定要求「A 须附等强论证方可替代」——我们的结论相反:A 的论证弱于 B,支持维持 B)。要点:选项 A(传输层分帧重组)把重组状态机放进 `nlos-ipc`/`nlos-system-control`(与 W82-A 写集交叠、TCB 内核路径增长),引入每连接重组缓冲的驻留内存与 stall 攻击面,且分帧在途的载荷**不可吊销**;选项 B 全部增量都在 TCB 外的旁路面(引用令牌表 + 逐块受限读),帧内永远只有引用。**请终裁确认 B。**

### 5.2 原语落点:建议独立第三通道 nlos.sabi.BlobReference(9→12);回退方案寄宿 tool_sandbox(9→11)

**为什么两个薄通道装不下 L-015**:引用原语需要 mint(发送侧)与 read(接收侧)两个**可分派的 wire 方法**——qq-agent 是外部进程,只能走 SABI wire,mint/read 都必须挂在某个 service 名下。逐个检查可挂靠处:
- 既有 9 通道:冻结零字节改动红线(§E.6.1.8 DAG「既有 9 通道零字节改动」)排除一切寄宿(含 SystemControl additive 扩列——那是改冻结 proto 文件字节)。
- LlmGateway:能力域是「模型与五维预算」,语音文件引用与其无关;qq-agent 今日只持有 ChannelControl 能力,为发一条语音被迫多持一张 LLM 网关能力,是最小权限的倒退。
- ToolSandbox:能力域是「沙箱工具执行」,同样错置。
- 只传消息不设 service 的共享 proto:消息可以共享,但方法分派需要 service 名——`sdk/python/nlos_sdk/common.py:43` `SABI_V1_CHANNELS` 未登记的 schema 在默认路径(未显式传 `supported_major` 覆盖)一律 `UNKNOWN_SCHEMA` fail-closed([common.py:74-76](../../sdk/python/nlos_sdk/common.py)),不可分派。

**建议**:新增第三通道 `nlos.sabi.BlobReference`,3 方法(均小帧 ≤64 KiB,字节永不进帧):
1. `mint_reference`:发送方声明(工作区内文件路径句柄、SHA-256、字节数、约束:读次数上限/单块上限/TTL)。服务层(库形态驻 `nlos-capability`,daemon 挂载归 INT,见 §11 B-4)校验:路径在声明工作区围栏内(词法 + canonicalize)→ **一次性打开文件并将 fd 钉进令牌记录**(平台允许处带 no-follow 语义)→ 全文摘要与长度核验 → 铸造引用令牌。**此后一切读取都从被钉 fd 出,路径永不重新解析**——堵死「mint 后把已验证文件换成指向围栏外的符号链接/硬链接」的换靶窗口(该窗口无竞速、TTL 全程开放,比沙箱证据自认的 canonicalize→open TOCTOU 更宽,必须结构性消除)。令牌同时绑定**逐块摘要清单**(每 64 KiB 块一个 SHA-256,随全文摘要入令牌记录),使读侧逐块可验而非只可期末全验(期末全验是先泄后验,不合格)。
2. `read_reference`:接收方持令牌按 (offset, len≤64 KiB) 分块读。**每块独立校验**:令牌权利/吊销/读计数/TTL + **holder principal == 解析方 principal**(逐块重验,使日志外泄的 `nlosref://` URI 天然失效——捡到 URI 者无法以属主身份完成握手) + 该块 SHA-256 与令牌摘要清单比对;字节从钉住 fd 的对应偏移读出。大文件 = 多次小帧顺序读,与 W82-A 会话循环天然契合;读取状态(游标)在服务端令牌侧,不在传输层——**无重组状态机**。
3. `release_reference`:主动弃用(卫生性;TTL 与读次数上限已保证收敛;被钉 fd 随之释放)。

**应用侧 URI 约定**:qq-agent 把 `voice_uri` 填为 `nlosref://<32hex>`。勘误(见 §14):`voice_uri` **不是** ChannelControl 的 proto 字段——它是 qq-agent 应用层 `OutboundReply` 消息的字段(E-DOGFOOD-001 §1),经 ChannelControl enqueue 的不透明 `bytes payload = 5`([channel_control.proto:88-98](../../schema/nlos/sabi/v1/channel_control.proto))传输。因此 **ChannelControl 零字节改动**的结论不变且更平凡:URI 方案是纯应用层约定,任何 proto 都不需要动。

**这是对规格的一个显式偏离,须维护者裁定**:§E.6.1.8 把「1 项大载荷传递原语」列为与「2 个薄通道」并列的第三项,但机械面(9→11)与 DAG 写集(`schema/nlos/sabi/v1/{llm_gateway.proto,tool_sandbox.proto,...}`)按两通道枚举。采纳第三通道则:冻结通道 9→**12**、DAG 写集增补 `blob_reference.proto`、方法总数 4+2+3=9。**回退方案**(若维护者否决第三通道):mint/read 寄宿 tool_sandbox(9→11 不变)——代价是能力域错置与 qq-agent 需为发语音多持沙箱能力,且未来 LlmGateway 大上下文引用也要复用同一错置入口;不推荐。

**代价**:v1.1 新增冻结面 +1 通道 / +3 方法;机械面所有落点把 11 改成 12(同一形状,§10 全列)。

### 5.3 能力权威演进:另立引用令牌表(additive migrate_v3),不动 capability_heads

**选项**:(i) 升 schema v3 于 heads(新 target 种类 + 新 rights 位);(ii) 另立引用令牌表。

**建议 (ii)。理由(全部来自勘察实证)**:
- (i) **不是纯增量**:[schema.rs:16-18](../../crates/nlos-capability/src/schema.rs) v1 DDL 把 `CHECK(target_kind IN (1, 2))` 与 `CHECK(rights > 0 AND rights <= 15)` 钉进表定义,而 heads 行被表族不可变触发器中的 2 个(`capability_heads_identity_immutable` 禁 UPDATE、`capability_heads_no_delete` 禁 DELETE,[schema.rs:67-95](../../crates/nlos-capability/src/schema.rs);表族共 10 个触发器)锁死——放宽 CHECK 需要 SQLite 表重建迁移(建新表→INSERT..SELECT→删旧→改名),是 v2(纯加表,[schema.rs:103-135](../../crates/nlos-capability/src/schema.rs))量级数倍的改动,且重建窗口本身是风险面。绝对安全宗旨下,为让一种新消费者而重建 TCB 核心表,不成比例。
- (ii) **完全复制 v2 先例**:在 capability-authority.db 内 additive 新增引用令牌表族(铸造/吊销/读取计数,全部 STRICT + 不可变触发器 + `PRAGMA user_version = 3`),不动任何既有表。令牌记录字段:`reference_id BLOB(16)`(SHA-256 域分隔派生,镜像 [lib.rs:821](../../crates/nlos-capability/src/lib.rs) `derive_id` 模式)、`file_digest_sha256 BLOB(32)`、`path_handle`(工作区相对路径句柄,非宿主绝对路径)、`holder_principal`、`size_bytes`、`max_reads`/`reads_count`、`max_chunk_bytes`、`valid_until_ms`、`minted_at_ms`、`revoked_at_ms`。DDL 保护测试按 [capability_authority.rs:472](../../crates/nlos-capability/tests/capability_authority.rs) / [call_limit_ledger.rs:616](../../crates/nlos-capability/tests/call_limit_ledger.rs) 先例各加 v3 类比。
- **衰减性不靠 rights 位也成立**:令牌自带约束列(读次数/块大小/TTL/持有者),v1.1 单级铸造(仅文件属主会话可 mint,接收方不可再委托)即满足「可衰减可吊销」;再委托链留作未来 additive(届时再加 parent 列)。ADR-0010/0022 一致性体现在:铸造命令按签名命令域(`b"nlos/capability/command/v1"` 模式新增 kind 4)经 principal 密钥签署、吊销单调、回执不可变。
- 附带修正:目标 id 全链硬钉 16 字节([schema.rs:10](../../crates/nlos-capability/src/schema.rs) 与 nominal_id),引用 id 天然 16B,零冲突。

**代价**:引用令牌与 capability 图是两族记录(语义上令牌=一次性衰减授权,不是图节点);若未来要求引用权进入通用委托图,需再评估 (i)——那时按需立项,不是 W82-B 的负载。

### 5.4 线上 slot 句柄 ↔ 权威 16B capability_id 映射(同形异义陷阱)

**实况**(勘察实证):wire 侧 [envelope.proto:37-40](../../schema/nlos/sabi/v1/envelope.proto) `CapabilityHandle { uint64 slot; uint64 generation; }` 是**进程/会话本地槽位索引**;权威侧 [model.rs:81-84](../../crates/nlos-capability/src/model.rs) `CapabilityHandle { capability_id: CapabilityId(16B); generation }` 是**全局持久身份**。同名不同物——今天两者之间**不存在任何已实现的映射机制**(nlos-capability 无任何 wire slot↔capability_id 映射代码;crate 内唯一的 "slot" 概念是 custody.rs 的 `CustodyKeySlot` 密钥槽,与能力句柄无关;`TaskExecutionBinding` 在生产代码中恒为 None,[control.rs:1819](../../crates/nlos-system-control/src/control.rs);`validate_sabi_request_context` 只校验 binding 六字段中的三个,[lib.rs:2884-2888](../../crates/nlos-schema/src/lib.rs))。

**映射设计(钉死)**:
1. **会话槽位表在 daemon 侧**:每条已握手连接维护 `slot(u64) → CapabilityId(16B)` 会话本地表,握手/导入时填充;wire slot 永远只是该表的索引,不承载任何全局语义。
2. **引用令牌不走 slot**:BlobReference 消息用**独立的 `bytes reference_id = 1`(16B)字段**,绝不复用 envelope 的 `CapabilityHandle` 消息类型——这是防同形异义的结构性隔离(编译期不同类型,不靠注释)。SDK 收到 mint 响应后自行把它登记进客户端本地槽位表(客户端槽位编号与 daemon 侧无关)。
3. **容量与逐出**:会话槽位表容量沿用语义层上界 `MAX_CAPABILITY_HANDLES = 64`([lib.rs:91](../../crates/nlos-schema/src/lib.rs));逐出 = LRU,**仅失效该会话的映射行,不触碰权威记录**(权威记录不逐出,镜像 custody「永久审计记录」纪律)。逐出时若有在途引用(该槽位正被某 read_reference 块序列使用),逐出推迟到块序列完成(pinning);被逐出后客户端以 reference_id 重新解析即可恢复。
4. **活引用 × 令牌吊销**:每块读取前校验令牌(吊销表 + 读计数 + TTL);中途吊销 ⇒ 下块起拒绝,已交付块即有界泄漏(≤ 已读块数 × 64 KiB),诚实记入语义注释。

**请裁定**:确认 5.1(B 终裁)、5.2(第三通道或回退)、5.3(另立令牌表)、5.4(映射规则)。

## 6. D6:current_epoch 维持 1;建 epochs/v1.1 黄金目录

### 6.1 current_epoch:建议**不随 v1.1 升 2**

**实证基础**(勘察逐条核过):
- `current_epoch` 唯一数据落点是 manifest 第 7 行;Rust 解析器只要求 ≥1 不钉具体值([deprecation.rs:823-828](../../crates/nlos-schema/src/deprecation.rs));钉死 1 的是测试 [version_matrix_and_deprecation.rs:338](../../crates/nlos-schema/tests/version_matrix_and_deprecation.rs)。
- 升级矩阵的 HostEpoch 1→2 是**对未来宿主演化的模拟**:step_c 用字符串替换 `.replace("current_epoch: 1", "current_epoch: 2")` 从 canonical YAML 推导 Host-N manifest([step_c_in_place_upgrade.rs:358-369](../../tests/stage_e_road_e001_upgrade_gate/src/step_c_in_place_upgrade.rs)),其前提就是 **canonical = 纪元 1 的 N-1 基线**。若 canonical 本身改成 2:该替换静默变 no-op、`parsed.frozen_channels_total != 9` 断言(:387,本就要改)之外,整个门禁的「1→2 演进」语义崩塌,须把模拟改为 2→3——这是对 H7 证据门禁的语义级返工,落在一个不属于任何 W82 车道写集的独立工作区 crate。
- `sabi_compat_diff.py` 对 epoch 前进 bump **零告警**(勘察实测:1→2 单独变化 → COMPATIBLE、零新条目;回归才 BREAKING,[sabi_compat_diff.py:647-656](../../tools/sabi-compat-diff/sabi_compat_diff.py))——两种取值都能过 diff 门,故选择权在设计口径,而口径应服务语义。
- 版本协商对 minor 与 current_epoch **双不敏感**:`adjudicate_schema_identity`([version_matrix.rs:83](../../crates/nlos-schema/src/version_matrix.rs) 定义,判体 :139-147)只看 identity.major vs policy.host_epoch([version_matrix.rs:160-168](../../crates/nlos-schema/src/version_matrix.rs)),minor 只是回显;host_epoch 是独立输入,Default 仍为 1([version_matrix.rs:32-40](../../crates/nlos-schema/src/version_matrix.rs))。升 2 不会带来任何协商收益。
- 语义正解:manifest 的 epoch 是**弃用窗口的代数**(供 `earliest_sunset_epoch >= deprecated_in_epoch + 2` 类计算),不是宿主发布号。新增通道不是弃用事件,不产生升代需求;新通道条目 `introduced_in_epoch: 1`(与 canonical 现值一致,解析器要求 `introduced_in_epoch <= current_epoch`,[deprecation.rs:992-998](../../crates/nlos-schema/src/deprecation.rs))。

**建议**:canonical 维持 `current_epoch: 1`;新通道与全部新条目 `introduced_in_epoch: 1`。真正的主系统代际升级(ROAD-E-001 实景)发生时,由那个波次统一升 2 并同轮返工升级门禁模拟——那时才是这个数字该动的时机。**文档口径**:全文统一「v1.1 = major 1 内 additive 扩列,contract epoch 仍为 1;HostEpoch 与 manifest current_epoch 是两个概念,前者是宿主代数(升级矩阵模拟对象),后者是弃用代数」。

**代价**:未来的真升级波次要同时动 canonical、:338 断言与 step_c 模拟(集中一次,而非现在动一半留隐患)。

### 6.2 epochs/v1.1 黄金目录:建议建(最小向量集)

**实证**:`check_golden_immutability.py` 对 epoch 目录**动态枚举**([:176](../../tools/check_golden_immutability.py)),v1.0 专属断言全部 `if epoch_name == "v1.0"` 门内(:239-247 等);新 epoch 目录只需自带合规 manifest.json(schema `nlos.golden.epoch_manifest.v1`、`immutable: true`、`sabi_major: 1`、`required_channels` = 其向量实际覆盖的通道集、逐向量双哈希 + 字节长、单行小写 hex 带尾 LF);`test_git_diff_rejects_mutation_of_existing_golden_and_allows_new_epoch`([test_golden_immutability.py:195](../../tests/conformance/schema/test_golden_immutability.py))已实证新 epoch 目录合法通过。

**建议**:W82-B 建 `schema/golden/epochs/v1.1/`:每新通道 2 条向量(一条典型正向请求、一条含未知非关键扩展的往返保持向量),manifest 按通用规则;`required_channels` 只列新通道(通用规则要求「= 向量覆盖集」,不是全量 11/12)。五语言解码扩展(Go/C# 探针当前只读 v1.0,需新增读取器)归 W82-D SDK 一致性;B 侧 Rust 黄金测试按 [epoch_v1_0_golden_corpus.rs](../../crates/nlos-schema/tests/epoch_v1_0_golden_corpus.rs) 模式新增 v1.1 段。**注意**:epochs/v1.0 的 11 向量与 7 个根级历史 hex **零漂移是 W82-B 硬验收门**,v1.1 向量绝不可误放进 epochs/v1.0/ 或 epochs/ 根(根下非目录实体即 `E_UNREGISTERED_GOLDEN_FILE`,[:180-183](../../tools/check_golden_immutability.py))。

**代价**:+1 目录、+6~8 向量、D 车道五语言解码扩展工作量;换来新通道与 v1.0 同等的字节锁定质量线。若维护者要压缩,v1.1 语料可整体降级为 D 车道交付物,但 B 的 proto 定稿质量线会失去独立字节锁——不推荐。

**请裁定**:确认 6.1 口径与 6.2 建目录。

## 7. D7:LLMOS_ROOT_CAPABILITY_SLOT 值格式(W82-C↔B 隐性契约,本文钉死)

**实况**:该环境变量目前**零实现**(全仓唯一提及是 §E.6.1.8 规划文本);且 "slot" 在能力域**没有任何已实现机制**(见 §5.4)——若 C 车道按字面把「槽位号」注入,将指向一个不存在的东西。

**建议钉死**:
- **值格式**:`^[0-9a-f]{32}$` —— 32 位小写十六进制,即 16 字节权威 `CapabilityId`(`nominal_id!` 的 [u8;16],[nlos-types/src/lib.rs:81](../../crates/nlos-types/src/lib.rs))。不含 slot 序号、不含 generation、无前缀、无分隔符。
- **语义**:标识「授予本受管进程实例的根能力」的权威 id。daemon 在该进程完成 `LLMOS_AUTH_ENDPOINT` 握手(身份密钥由 `LLMOS_PROCESS_IDENTITY_SEED` 派生)后,校验该能力的 **holder principal == 握手派生 principal** 才把它物化进会话槽位表(§5.4)——**值本身零授权**:只持 id 不持身份密钥什么都做不了(fail-closed),id 是 128 位不可猜的派生值(镜像 [lib.rs:821](../../crates/nlos-capability/src/lib.rs) SHA-256 域分隔派生)。
- **generation 不进值**:吊销会推进 generation,进程启动时应解析「当前代」;把代数烧进环境变量会在吊销-重启竞态下自相矛盾。
- **注入惯例**:沿用 `DesktopSessionEnv` 的有序 (NAME, VALUE) 注入模式([nlos-up/src/lib.rs:454-491](../../crates/nlos-up/src/lib.rs)),C 车道新增该键即可,值由 daemon 侧签发步产出。

**C 车道验收联动**(供对表):未通过 holder 校验的握手中该 id 不可物化;进程树回收时该能力随之吊销(§E.6.1.8 托管约定 C 验收门)。

**代价**:W82-C 对表单向锁定此格式;值内不含 generation 的有界后果 = 吊销-重启竞态下 C 侧必须解析「当前代」而非期望固化代数;daemon 侧需有签发步产出该值(ADR-0022 模式的每进程实例化)。

**请裁定**:确认值格式与语义,W82-C 按此对表。

## 8. D8:五维预扣 driver 层组合方案(nlos-driver-llm-http 内,nlos-resource 零改动)

### 8.1 现状锚点(全部勘察实证)

- 现流是「三维只读准入 → 发包 → 事后记账」窗口模式:准入只查 ApiCalls/TotalTokens/CostMicrocredits 三维([shim.rs:611-614](../../crates/nlos-driver-llm-http/src/shim.rs)),Prompt/Completion 两维从不预检;发包后 `consume_llm_tokens` 补账([shim.rs:633-664](../../crates/nlos-driver-llm-http/src/shim.rs));无预留、无退款——失败路径零记账(provider 侧花费丢失),并发下先过准入者的补账可能在事后撞 `LlmTokenBudgetExhausted`(超支已发生,用量记录丢失)。这就是 TOCTOU 窗口。
- nlos-resource LLM 轨道**没有** reserve/settle/delegate(六原语:create_account [:1139]、create_llm_token_budget [:2481]、update_llm_token_budget [:2573]、consume_llm_tokens [:2724]、inspect [:2933]、list [:2968]);consume 严格只增、拒 api_calls==0([lib.rs:2745-2747](../../crates/nlos-resource/src/lib.rs)),consumed_* 被 SQLite 触发器钉死单调([schema.rs:885-898](../../crates/nlos-resource/src/schema.rs));update 只能降 limit 且不得低于 consumed([lib.rs:2611-2627](../../crates/nlos-resource/src/lib.rs))。**真 reserve-then-settle 机制(quote/upper_bound→CAS 预扣→单调用量→finalize 退款)只存在于任务资源族**([lib.rs:1248](../../crates/nlos-resource/src/lib.rs) reserve、[1489-1503](../../crates/nlos-resource/src/lib.rs) 单调校验、[1888](../../crates/nlos-resource/src/lib.rs) `refund_credit = upper_bound - final_usage`)。任务资源族引不进 LLM 轨道——那要改 nlos-resource(非 B 写集)。

### 8.2 推荐组合:门内影子预留账本 + 权威 consume 为唯一持久扣减

**核心决策**:「预留」不落权威账本(落了就退不回来),落 **driver 内持久 journal**;权威的 `consume_llm_tokens(actual)` 仍是唯一持久扣减,其 fail-closed 包络校验([lib.rs:2800-2816](../../crates/nlos-resource/src/lib.rs))仍是最终守门。

**结构**(全部在 nlos-driver-llm-http 内,零新依赖、零跨 crate 写):
1. **预留 journal**(std::fs 追加式记录文件,域分隔摘要锚定,镜像 NLOS-HOST-R1 的零依赖纪律):每条预留记录 `(reserve_key 16B = derive(SABI idempotency_key), account/会话, 五维 quote, deadline, state ∈ {PENDING, SETTLED, SETTLE_REJECTED, ABANDONED})`。启动重放重建在途集;journal 是对账源,不是账本。**持久化契约(跨平台钉死)**:记录与文件 fsync 用 `File::sync_all`(std,三平台皆可);journal 创建时的目录项 fsync 在 Unix 走 std,Windows 无 std 原语——按本仓 PR #17 教训,以 `OpenOptionsExt::custom_flags(0x0200_0000 /* FILE_FLAG_BACKUP_SEMANTICS */)` + `.write(true)` 的裸常量实现(有意不复用 nlos-artifact 的 windows-sys 先例以避免动 INT 保留的 root Cargo.lock),或在文档中显式接受「Windows 无目录 fsync」并留诚实注记——二选一,实现时钉死。
2. **五维报价(服务端价目表)**:per-model 常量表(µcredits/1k tokens 等;先例:openai-compat-driver `cost_per_kilotoken_microcredits` 的 ceil 计价,[metering.rs:134-137](../../examples/ecosystem-drivers/openai-compat-driver/src/metering.rs))。quote 五维 = prompt 估算(输入字节,`estimate_tokens_from_utf8_bytes` 已有,[shim.rs:805-808](../../crates/nlos-driver-llm-http/src/shim.rs))**加保守系数**(现公式 `(bytes+3)/4` 对 CJK 低估——按 D2 口径 64 KiB CJK ≈ 1.6-2.1 万字符 ≈ 1.6 万 token,远大于 bytes/4;gateway 侧对 CJK 占比高的输入按字符数计 token 上界,或对 quote 乘 ≥2 的保守因子,实现时以实测校准)+ completion 上界(`req.max_tokens`)→ total 上界 + api_calls 1 + cost 上界(价目表 × token 上界)。**wire 方法上根本没有客户端 cost 字段**——llm_gateway.proto 的 llm_chat 请求不含任何 cost 声明,服务端价目表是唯一计价源(结构上杜绝自报,优于校验)。
3. **准入(硬熔断)**:`effective_remaining(d) = remaining_authority(d) − Σ PENDING_quote(d)`;五维任一 `quote > effective_remaining` ⇒ typed `BudgetExhausted`(维度归因按 ALL 顺序,[lib.rs:609-615](../../crates/nlos-resource/src/lib.rs))⇒ **零发包**。并发竞态被 journal 内单点串行收口(gateway 服务层内一把锁)。**旁路写权威者的诚实边界**:现实存在第二写者(desktop `keypool_bridge` 直接开 ResourceAuthority,[keypool_bridge.rs:582-590](../../desktop/src-tauri/src/keypool_bridge.rs))——本设计以**派生域隔离**钉死:gateway 账户 id 用 gateway 专属派生域(区别于 desktop/nlos-up 的 trial 域),使既有旁路写者**按构造不可能**写到 gateway 账户;若未来出现故意瞄准 gateway 账户 id 的旁路写者,则权威包络仍保证账本不超限,但**真实开销可能越限**且幻影余量会留存——此残余如实写入 proto 语义注释,不在 v1.1 声称消除。
4. **结算**:成功 ⇒ `consume_llm_tokens(actual)`(actual 来自 provider 响应 usage 字段,`extract_or_estimate_usage` 已有,[shim.rs:735-803](../../crates/nlos-driver-llm-http/src/shim.rs);cost = 价目表 × actual,非自报)。consume 幂等(同键同载荷重放回执,[lib.rs:2729-2744](../../crates/nlos-resource/src/lib.rs)),崩溃于 consume 与 SETTLED 之间重放不双扣。actual 超包络被拒 ⇒ journal 置 **SETTLE_REJECTED**(保留 actual 五维值与供给方回执指纹,供对账与运维可见——不是「零持久写」了事;现状代码同样会在结算点撞 `LlmTokenBudgetExhausted` 而丢记录,[shim.rs:653-661](../../crates/nlos-driver-llm-http/src/shim.rs),本设计修复该丢失)。journal 置 SETTLED 于成功路径。
5. **失败退款**:HTTP/解析失败 ⇒ **零扣减**:journal 置 ABANDONED,PENDING 出表,预算分毫未动(预留从未落账,所以"退款"就是弃单)。多槽 failover([keypool.rs:813-888](../../crates/nlos-driver-llm-http/src/keypool.rs))同属一次调用一个预留,`api_calls` 记 1 不变(现状语义);**诚实注记**:失败尝试在供给方侧仍被计费(prompt 每 attempt 都发生),该真实开销与我们的零扣减并存——计入崩溃窗口边界(见 6)。
6. **崩溃窗口**:dispatch 与 settle 之间崩溃 ⇒ journal 留 PENDING;启动清扫把超 deadline 的 PENDING 置 ABANDONED 并记 `EffectUnknown` 对账行。**诚实边界(修正过紧的旧表述)**:崩溃丢失的是「供给方侧已发生的真实用量」,其界 = 在途调用的 **actual 用量**,可越过 quote——越出部分 = (i) CJK 估算误差(见 2 的保守系数缓解)与 (ii) 多槽 failover 各尝试的 prompt 计费(见 5)。它仍**有界**(在途调用数 × 各自 actual 上界),且每次都落 journal 对账行可见;不是无界漏记。
7. **account/operation/idempotency 派生(认证主体派生,非客户端自报)**:gateway 会话建立时 `account_id` 由 daemon 按**已认证 principal** 派生(镜像权威 `ResourceAccountId::from_bytes(id16(...))` 的域分隔派生模式,[lib.rs:1152](../../crates/nlos-resource/src/lib.rs)),子会话(衰减)各得派生 account;`operation_id`/`idempotency_key` 从 SABI envelope 的 128-bit 幂等键与 request 摘要域分隔派生(先例:openai-compat-driver 从 request digest 派生使 UNCERTAIN 重放同键,[metering.rs:101-112](../../examples/ecosystem-drivers/openai-compat-driver/src/metering.rs))。这正是对现状信任缺口的修复:今天 `MeteredBudgetContext` 的三键由调用方随意供给(keypool_bridge 以显式字段直填,[keypool_bridge.rs:582-590](../../desktop/src-tauri/src/keypool_bridge.rs)),crate 内零校验。该结构体保持形状不动(其两个外部构造点 [nlos-up/src/lib.rs:1092-1098](../../crates/nlos-up/src/lib.rs)、[keypool_bridge.rs:582-590](../../desktop/src-tauri/src/keypool_bridge.rs) 是结构体字面量,加字段会破它们的编译,而它们不在 B 写集)——gateway 路径用**新的**上下文类型包裹它。
8. **每主体速率与行数上限(DoS 有界,与 BlobReference 对称)**:`record_external_usage` 每会话每窗口(建议 60s)追加行数上限(建议 256,超出 typed 拒绝而非静默追加);`attenuate_budget` 每父账户**存活子账户数上限**(建议 16;create_account 本身无账户数上界,[lib.rs:1139](../../crates/nlos-resource/src/lib.rs),故此上限必须由 gateway 层持有)。两者计入 §12.3 有界性表与 B-2 验收负向测试。

### 8.3 attenuate_budget 的父≥子不变量(自证设计)

衰减 = 在 journal 讑账下为子会话 `create_llm_token_budget`(子 account,limits ≤ 授予额),授予瞬间记 journal 转移行;**父账本不动**(LLM 预算本就是各 account 独立包络,无信用联动——勘察实证「LLM budgets never touch available_credit」)。父子约束在 gateway 准入处双查:子调用须同时满足 子包络(effective)与 **journal 树有效余量**(父授予额 − Σ子已结算 − Σ子 PENDING)。

**自证不变量测试**(验收门原文「自证父≥子不变量」):性质测试对任意操作序列断言三式——
1. 权威层(逐 account):`consumed_d ≤ max_d` 全维(权威自身保证,测试兜底断言);
2. journal 树:`Σ(子 settled_d + 子 pending_d) ≤ 父授予额_d ≤ 授予时父 effective_remaining_d`;
3. 端到端:`父 effective_remaining_d ≥ 0` 恒成立且每次发包前 quote 五维全部通过(含崩溃注入:任意点 kill -9 重启后重放 journal,1/2 式仍成立,孤儿 PENDING 归 ABANDONED)。

**代价**:driver 内新增 journal 文件与清扫逻辑(复杂度诚实标注:约一个模块 + 性质测试);旁路直写权威者不受 gateway 预扣管辖(权威包络仍终守,如实写入语义注释)。

**请裁定**:采纳 8.2 组合与 8.3 不变量测试定义。

## 9. D9:guest 工具 ABI、取消映射与沙箱执行模型(nlos-wasm-sandbox 内)

### 9.1 执行模型:v1.1 取 per-call 宿主进程;常驻宿主延后(附实测计划)

**现状实证**:沙箱是双层完整的一次性执行——guest 入口硬钉 `() -> i32`([isolate.rs:239-245](../../crates/nlos-wasm-sandbox/src/isolate.rs));宿主子进程 `env_clear()` + 显式 argv + 墙钟死线 kill([process.rs:150-155, 231-245](../../crates/nlos-wasm-sandbox/src/process.rs));`run_guest` 每进程恰一次、无多轮([host_bin.rs:261-266](../../crates/nlos-wasm-sandbox/src/host_bin.rs));WASI 白名单**实际 14 个导入**(勘误:证据文档写 13 但其括号列表就是 14 个;`add_to_linker` 共 14 处注册,[wasi_allowlist.rs:412-886](../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs) 逐个核过——本文档一律按 14 引用,顺带建议 INT 勘误证据文档);stdin 两层皆死(L1 `Stdio::null()` + WASI fd 0 恒 EOF);入口名可按调用指定(`--entry` 任意);墙钟 kill 由**父进程**实施、默认 profile 的 epoch deadline 为 0(不设, [profile.rs:139](../../crates/nlos-wasm-sandbox/src/profile.rs))。**零生产调用方**——工具 ABI 将是本 crate 首个真实集成。

**建议 per-call**:每次 `invoke_tool` = 一个宿主子进程(入口名按工具导出约定),零新协议、零新攻击面、逃逸对抗矩阵(13 项集成 + 单元符号链接分支)直接覆盖。LLM 节奏的工具调用对 ~ms 级 spawn 开销不敏感。**孤儿进程防线(新增设计,回应红队)**:墙钟 kill 在父进程侧,daemon 中途崩溃会孤儿化 L1 宿主进程——因此工具默认 profile **必须设非零 epoch deadline**(建议默认 30s tick 预算;epoch 计时线程在 L1 宿主进程**内部**,[isolate.rs:91-125, 215-226](../../crates/nlos-wasm-sandbox/src/isolate.rs),父进程死不终止它)——被孤儿化的 guest 到点**自灭**,磁盘写入与 CPU 都被时间封顶。OS 级孤儿回收(Unix 进程组 + PDEATHSIG 类、Windows Job Object)作为 B-3 卫生项与 INT 挂载时一并评估,B-3 验收含「daemon 中途被杀 ⇒ 孤儿在 epoch 预算内自灭」负向测试。
**常驻宿主延后**,理由与门槛:常驻需宿主侧多轮请求协议 + 逐请求 profile 切换;若还要 guest→host 工具回调则必须扩 `add_to_linker`——那是该 crate 声明的**单点注册安全不变量**([wasi_allowlist.rs:406-410](../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs) "if and only if"),属 ADR-0015 级决策,且「库永不载入不可信模块进调用方进程」边界要重论证。**实测计划(写给常驻的复活条件)**:B 车道交付时附 spawn+run 基准(三平台,典型工具载荷),若实测 p95 每调用开销 > 50ms 且有真实消费者抱怨,再立独立车道按上述门槛评审常驻方案;届时 NLOS-HOST-R2 报告扩展(现版本标签已留缝)一并设计。

### 9.2 工具目录与 JSON ABI(零依赖、不碰 WASI 白名单)

- **目录导出**:约定 guest 以专用入口导出目录——`__nlos_tool_catalog__` 入口(或 `--entry` 调用)把目录 JSON 打到 stdout 后退出;宿主缓存目录(带 guest wasm SHA-256 键)。**不读 custom section**(host 现无解析)、**不加 host 导入**(不动白名单单点)。`list_tools` 返回目录(逐工具:name、JSON-Schema 摘要、EffectClass 四分类声明——复用生态样板的四类语法与校验规则,[effects.rs:22-105](../../examples/ecosystem-drivers/ecosystem-driver-kit/src/effects.rs):变更类必须声明 reconcile=same-idempotency-key、COMPENSATABLE 必须有补偿器);未声明工具 fail-closed 拒绝(镜像 [adapter.rs:249-289](../../examples/ecosystem-drivers/mcp-bridge-driver/src/adapter.rs) 的 UnknownTool 纪律)。
- **参数进 / 结果出**:小参数 = 单 argv 字符串(JSON);**大参数 = 预打开文件**(L-015 引用解析后落工作区临时文件,`--preopen` 只读授予——避开 ARG_MAX 与 stdin 死端,勘察确认此为自然缝)。结果 = stdout 字节(service 层解析为 JSON);**stdout 累计字节必须新加上界**(勘察实证:现状累计 stdout 无界、仅饱和计数,[wasi_allowlist.rs:179-182](../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs);单次 fd_write 才有 4 MiB iovec 上界)——B 在 `process.rs` 增流式有界读取变体:默认 `MAX_TOOL_STDOUT_BYTES = 256 KiB`,超限即 kill + typed `ToolOutputTooLarge`,更大结果走「写预打开目录 + 返回摘要引用」(与 L-015 闭环)。**同理新增逐调用写入字节预算** `MAX_TOOL_WRITE_BYTES`(建议默认 8 MiB):在 WASI 层对预打开文件的 `fd_write` 累计计数,超限 kill + typed 错误——勘察实证:**仓内不存在任何工作区/磁盘配额**(燃料/epoch 之外无字节界),stdout 上界只能把大输出挤向预打开文件,没有写入预算则该路径可写穿磁盘;工作区级配额的缺失作为显式已知边界登记(维护者知悉接受),不在 v1.1 声称补齐。
- **JSON 编解码与哈希原语(零依赖约束的完整披露)**:JSON 手写有界 RFC 8259(先例:nlos-driver-llm-http 手写 JSON、ecosystem-driver-kit `MAX_JSON_DEPTH = 64`)。**摘要原语须显式决策**:本 crate 唯一依赖是 wasmtime 50.0.1(无 SHA-256),而 journal/目录缓存的摘要语义对齐 nlos-store 的 SHA-256——三选一:**(推荐)请 INT 以 W80-0-SCAFFOLD 式前置提交预锁 `sha2` 为本 crate 依赖**(与 wasmtime 预锁同型);回退 (a) 手写 SHA-256(#![forbid(unsafe_code)] 可审计,须附公开测试向量并计入复杂度预算)或 (b) 降级为文档化的非密码学 128-bit 摘要(带碰撞风险诚实注记)。不得留空不指定。
- **燃料/内存配置**:逐工具 profile(默认 fuel 1_000_000 / 16 页 1 MiB / **epoch deadline 非零(§9.1)**,可按目录声明覆设,受 crate 硬顶约束)。

### 9.3 取消映射与效果建模(幂等走持久注册表)

- **取消路径**:`OperationControl.cancel`(冻结通道)→ tool_sandbox 服务层把该 operation 置 `CancelRequested` 并**杀子进程**(L1 `child.kill()`,即现有墙钟 kill 同一原语)。杀丢失结构化报告(勘察实证:kill 后无 NLOS-HOST-R1)——所以效果**显式建模**而非猜测:
  - kill 早于 guest 启动完成 ⇒ `CancelledBeforeEffect`;
  - kill 时已运行 ⇒ **`EffectUnknown`**(不能证明工具是否已写工作区文件)——与 `nlos-operation` 状态机([lib.rs:41-50](../../crates/nlos-operation/src/lib.rs))及任务槽 `EffectUnknown never silently resolves` 纪律([effect.rs:104-111](../../crates/nlos-task/src/effect.rs))同语义;wire 词汇直接用已冻结的 `SABI_ERROR_CODE_CANCELLED/PARTIAL/EFFECT_UNKNOWN` + `RetryDirective`([envelope.proto:59-87](../../schema/nlos/sabi/v1/envelope.proto))。对账(reconcile)是唯一出路:比对工作区前后摘要可判 `EffectClosed/ConfirmedNoEffect`——v1.1 提供目录级摘要对账,多效果工具的细粒度 `PartialEffect`(至少一个必要槽已满足,[model.rs:1093-1110](../../crates/nlos-task/src/model.rs))留待真实多效果工具出现后 additive。
  - deadline 到期 = 同一路径(杀 + `Deadline`/`EffectUnknown`;先例:[nlos-directory-chain.rs:721-737](../../crates/nlos-ipc/src/bin/nlos-directory-chain.rs) deadline_after_dispatch → cancel + EffectUnknown)。**不做死线收紧**(勘察实证:epoch ticks 生成时钉死、无跨进程重置通道;死线即墙钟 kill,收紧是常驻宿主的话题)。
  - envelope 的 `TaskExecutionBinding.cancel_epoch` 在 wire 上是死字段(生产恒 None、校验器只查六分之三字段,见 §5.4 实证)——v1.1 取消**不依赖**它,走 OperationControl + 服务内 operation 状态机(镜像 [nlos-operation lib.rs:317-368](../../crates/nlos-operation/src/lib.rs) 的 cancel_epoch ticket 纪律)。
- **幂等注册表(持久,不照搬内存 PendingCallFence)**:勘察实证 `PendingCallFence` 是进程内 `BTreeMap`([effects.rs:332](../../examples/ecosystem-drivers/ecosystem-driver-kit/src/effects.rs)),真崩溃连账本一起蒸发,且其「provider 证据」是自造的([adapter.rs:346-352](../../examples/ecosystem-drivers/mcp-bridge-driver/src/adapter.rs) 拿自家 JSON-RPC 计数器当回执)、replay 不返回结果内容——三处都不能抄。替代:nlos-wasm-sandbox 内新增 `ToolOperationStore` trait + 默认实现 = **追加式持久 journal**(std::fs、fsync、域分隔摘要;语义镜像 [nlos-store](../../crates/nlos-store/src/lib.rs) `begin_idempotent_operation`/`complete_idempotent_operation`:同 scope+128-bit 键+同请求摘要 ⇒ 重放 `DurableCallResult`(含结果内容或其内容寻址),异摘要 ⇒ `IdempotencyConflict`;Dispatched/EffectUnknown 期间同键重试被拒——`KABI-UNCERTAIN-001` 禁盲重做)。INT 合流时可换 `SqliteOperationStore` 适配实现(daemon 已开 operations.sqlite3,[daemon.rs:1112](../../crates/nlos-system-control/src/daemon.rs)),trait 缝留在 B 侧定义。
- **MCP 桥关系**:tool_sandbox v1.1 只服务**沙箱内 wasm 工具目录**;外部 MCP server 桥接仍是 mcp-bridge-driver(DriverProvider 面)的职责,不重叠、不内嵌(勘察实证两者今天零接线,保持解耦)。

**验收联动**(§E.6.1.8 W82-B 行):「双层沙箱工具隔离单测全绿」= 既有 13+1 逃逸矩阵全绿 + 新增工具 ABI 正负向(stdout 超限、**写入预算超限**、未声明工具、取消三态、幂等重放/冲突、崩溃对账、**daemon 中杀孤儿自灭**)。

**代价**:每次 invoke_tool 一个宿主进程 spawn(实测计划阈值 50ms p95 为常驻方案的复活判据);stdout >256 KiB / 写入 >8 MiB 的工具必须改走「预打开目录 + 摘要引用」;摘要原语取 INT 预锁路线则 B 对屏障有一次单向依赖(回退手写可自足)。

**请裁定**:采纳 9.1(per-call + epoch 自灭防线 + 常驻延后实测计划)、9.2 ABI(含写入预算与摘要原语三选一)、9.3 取消与幂等设计。

## 10. D10:机械面清单(9→N 的全部落点;推荐 N=12,回退 N=11)

### 10.1 联锁值与测试断言(两组「9」必须分辨——按 N=11 写,推荐路线全部 11→12 同形)

**第一组:活注册表/活 manifest 的计数(必须改)**:

| # | 落点 | 现状 | 改法 |
|---|---|---|---|
| 1 | [deprecation-manifest.yaml:9](../../schema/nlos/sabi/v1/deprecation-manifest.yaml) | `frozen_channels_total: 9` | → 11(推荐 12);第 73 行后追加通道块(name/major:1/minor:0/frozen:true/空 reserved)与符号条目(全部 `introduced_in_epoch: 1`、`ACTIVE`);**current_epoch: 7 行不动**(D6) |
| 2 | [version_matrix_and_deprecation.rs:343-344](../../crates/nlos-schema/tests/version_matrix_and_deprecation.rs) | `frozen_channels_total, 9` / `channels.len(), 9` | 两处 → 新值;:345 `entries.len() >= 9` → `>= 新值`;测试函数名含 "nine" 顺带改名;**别动 :338 的 `current_epoch, 1`**(D6) |
| 3 | [compatibility.rs:200](../../crates/nlos-schema/tests/compatibility.rs) | `registry.len(), 9` | → 新值 |
| 4 | [compatibility.rs:297](../../crates/nlos-schema/tests/compatibility.rs) | `registry.len(), 9, "expected all 9 ..."` | → 新值 + 改消息;**同文件 :265-275 的 frozen_names 九名数组追加新 SABI_*_SCHEMA 常量** |
| 5 | [epoch_v1_0_golden_corpus.rs:49](../../crates/nlos-schema/tests/epoch_v1_0_golden_corpus.rs) | `manifest.channels.len(), 9`(canonical manifest 的通道数,非向量数) | → 新值;:51-61 `all_channels` 九名数组追加;:9-23 导入新增常量;文件头注释 9/9 → N/N。**注意 11 向量数不动**(那是 v1.0 语料的事) |
| 6 | [step_c_in_place_upgrade.rs:387](../../tests/stage_e_road_e001_upgrade_gate/src/step_c_in_place_upgrade.rs) | `parsed.frozen_channels_total != 9`(Host-N 头不变量末子句) | → `!= 新值`。该 crate 独立工作区([Cargo.toml:1-3](../../tests/stage_e_road_e001_upgrade_gate/Cargo.toml) `[workspace] members=["."]`),主 workspace 测试不含它;由主 CI `verify` 作业 :136-139 经 `run_road_e001_upgrade_matrix.py` 跑(`--manifest-path` 直指其 Cargo.toml)——**不更新则三平台主 CI 必红** |

**附加清扫(非门禁,防陈旧表述误导后续审计)**:7) 注释/文档口径 sweep——manifest 头注释(:4 "9/9")、[lib.rs:42/:180](../../crates/nlos-schema/src/lib.rs) 与 [deprecation.rs:487](../../crates/nlos-schema/src/deprecation.rs) 的 "9 channels"、[messages.go:37](../../sdk/go/sabi/messages.go)/[Messages.cs:38](../../sdk/csharp/llmos-sabi-probe/Messages.cs) 镜像表注释、envelope.py/envelope.ts 相邻注释、[step_c:346](../../tests/stage_e_road_e001_upgrade_gate/src/step_c_in_place_upgrade.rs) "9 frozen channels"、desktop Studio 界面 "9/9 FROZEN" 徽标(desktop 不在 B 写集,登记 INT);manifest 头注释可安全改(两套解析器都剥注释,不影响 step_c 字面替换)。8) 函数名 sweep——[version_matrix_and_deprecation.rs:335](../../crates/nlos-schema/tests/version_matrix_and_deprecation.rs) 与 [compatibility.rs:293](../../crates/nlos-schema/tests/compatibility.rs) 两个含 "nine" 的测试函数名顺带改名。

**lib.rs 主体**(同一提交):REGISTRY([:216-226](../../crates/nlos-schema/src/lib.rs))追加 2-3 个 descriptor const(镜像 [SABI_CHANNEL_CONTROL_V1 :208-214](../../crates/nlos-schema/src/lib.rs) 模式,minor 记 0,doc 注释记 W82-B 出处——SystemControl 的 minor 溯源注释块 [:139-157](../../crates/nlos-schema/src/lib.rs) 是惯例)+ REGISTRY 数组元素;新 `SABI_*_SCHEMA` 名常量(镜像 :47-49);新 `MAX_*_PAYLOAD_BYTES`(D2);每通道 `*_schema_identity()` + `validate_*_identity()` + 逐方法 encode/decode 有界函数族(镜像 [:1973-2462](../../crates/nlos-schema/src/lib.rs) ChannelControl 9 方法 × 4 函数 = 36 函数的形状;LlmGateway 4 方法、ToolSandbox 2 方法、BlobReference 3 方法)。

**第二组:v1.0 语料钉死的计数(绝不能改——改了即篡改不可变语料)**:Go [golden_test.go:543-544](../../sdk/go/sabi/golden_test.go) covered==9、C# [Program.cs:617-618](../../sdk/csharp/llmos-sabi-probe/Program.cs) 向量 11/通道 9(:606 legacy 7)、[envelope.py:1073/1076](../../tests/conformance/schema/envelope.py) required_channels==9 / 向量==11、[envelope.ts:1332/1337](../../tests/conformance/schema/envelope.ts) 同形、[test_golden_immutability.py:43-44](../../tests/conformance/schema/test_golden_immutability.py) v1.0 channel_count==9、check_golden_immutability.py 的 `CANONICAL_V1_0_CHANNELS`(:17-27,仅对名为 v1.0 的目录生效)与 `EXPECTED_LEGACY_ROOT_COUNT=7`。**把两组当同一批 9→N 全局替换会炸掉第二组**——这是本机械面最尖的坑。

### 10.2 生成面与门禁面

- **新 proto 禁 service 块**:[build.rs:10-14](../../crates/nlos-schema/build.rs) `LocalRpcServiceGenerator` 断言包内唯一 service 就是 LocalRpcService/Exchange——新 proto 只写 message(8/9 既有 proto 均如此,方法以 Rust 函数族 + envelope service/method 字符串表达)。
- **build.rs:48-58 protos 数组**:手动追加新 proto 路径(Rust 侧 prost 不自动发现;buf 侧 `inputs: directory: schema` 自动发现,无需登记)。
- **gen 生成门**:`npm run schema:generate`(buf generate + normalize 脚本)重新生成并**提交**新增 gen 文件(Python pb2 × N + TS pb × N);既有 18 文件(9+9)必须字节不变([check-generated-schema.mjs](../../tools/check-generated-schema.mjs) 以 `git status --porcelain -- gen` 零漂移为准;normalize 会剥行尾空白+统一尾 LF,新文件同受其规范)。**buf 版本永不动**:buf.gen.yaml 钉 `protocolbuffers/python:v33.4` + `bufbuild/es:v2.13.0`。
- **buf breaking PR 门**([rust-cross-platform.yml:121-123](../../.github/workflows/rust-cross-platform.yml)):`buf breaking --against .git#branch=origin/<base>,subdir=schema`(FILE 规则)对纯新增文件天然放行、对任何既有 proto 改动报警——这是纯增量的机器门。
- **compat-diff 证据协议(CI 门只做恒真的自 diff,纯增量结论须手工双 YAML 归档)**:CI 里 `test_sabi_compat_diff_gate.py:53` 以同一 canonical 文件既当 baseline 又当 candidate(自 diff恒 compatible),**证明不了 v1.0→v1.1 演进**。B 车道必须手工跑并归档 JSON 报告为证:
  ```bash
  BASE=$(git merge-base HEAD origin/main)   # 或立项基线 SHA
  git worktree add /tmp/w82b-base "$BASE"
  python3 tools/sabi-compat-diff/sabi_compat_diff.py \
    /tmp/w82b-base/schema/nlos/sabi/v1/deprecation-manifest.yaml \
    schema/nlos/sabi/v1/deprecation-manifest.yaml \
    --baseline-proto-dir /tmp/w82b-base/schema/nlos/sabi/v1 \
    --candidate-proto-dir schema/nlos/sabi/v1 \
    --json > <证据路径>/w82b-v11-compat-diff.json
  git worktree remove /tmp/w82b-base
  ```
  预期:exit 0 / compatible / 新通道与新条目全部 `ADDITIVE_SAFE`、零 BREAKING(勘察已按规则预演:新冻结通道 → ADDITIVE_SAFE,新 ACTIVE 条目 → ADDITIVE_SAFE)。manifest 语法须留在工具的手写 YAML 子集内(顶层标量非负整数、reserved 列表单行内联)。
- **黄金不可变门**:`python tools/check_golden_immutability.py` 常跑;v1.0 零漂移另以 `--check-git-diff <基线SHA>` 审计态跑一次归档(v1.1 新向量只进 epochs/v1.1/,误放 epochs/v1.0/ 或 epochs 根即 `E_UNREGISTERED_GOLDEN_FILE`/`E_GIT_DIFF_IMMUTABLE_VIOLATION` 拦截)。
- **SDK 四语言表 + 集合/计数相等断言松绑(最尖隐性耦合,勘察+红队复核发现,共六处)**:新通道必须在 `SABI_V1_CHANNELS` 登记,否则运行期默认路径 `UNKNOWN_SCHEMA` fail-closed(Python [common.py:43-55, 74-76](../../sdk/python/nlos_sdk/common.py);TS [common.ts:42, 118-120](../../sdk/typescript/src/common.ts);Go [messages.go:37-49](../../sdk/go/sabi/messages.go);C# [Messages.cs:38-50](../../sdk/csharp/llmos-sabi-probe/Messages.cs))。但有**六个**断言把不可变 v1.0 语料与活 SDK 表做相等比较,SDK 表一扩即红,而 v1.0 语料绝不能改:
  1. [envelope.py:1072](../../tests/conformance/schema/envelope.py) required_channels 集合 == SDK 表键集;
  2. [envelope.py:1077-1079](../../tests/conformance/schema/envelope.py) v1.0 **向量覆盖通道集** == SDK 表键集(红队补遗——B 阶段仍绿,python 表一扩即在 D 车道爆);
  3. [envelope.ts:1328-1332](../../tests/conformance/schema/envelope.ts) deepEqual 同 1;
  4. [envelope.ts:1338-1345](../../tests/conformance/schema/envelope.ts) 向量覆盖 deepEqual 同 2;
  5. [Program.cs:592](../../sdk/csharp/llmos-sabi-probe/Program.cs) `required_channels.Count == 表.Count` **计数**相等(:589 的逐通道 Contains 是天然子集语义,可不动);
  6. [golden_test.go:508-509](../../sdk/go/sabi/golden_test.go) 同 5 的计数相等(:512-513 成员检查可不动)。
  **松绑形态**:1/3 → 「v1.0 required_channels ⊆ SDK 表」;2/4 → 「v1.0 向量覆盖通道集 ⊆ SDK 表」;5/6 → 删计数相等行(或改子集)。**六处必须与 SDK 表扩表同一逻辑变更落地**(B-1 内完成松绑,先于 D 的表扩),否则 D 车道将在无写集归属的文件里爆红。**执行面诚实注记**:六处中只有 1-4 进 CI(`python -m unittest discover -s tests/conformance/schema`,[rust-cross-platform.yml:89](../../.github/workflows/rust-cross-platform.yml));Go/C# 套件**不在任何 CI 步骤内**——B-1 验收清单须加手工跑 `go test ./sdk/...` 与 C# 探针一次,防「改了表、忘了松绑」的隐性红。

### 10.3 写集处置(跨车道与未归属文件)

| 文件/组 | 现属 | 处置建议 |
|---|---|---|
| schema/nlos/sabi/v1/*.proto、deprecation-manifest.yaml、crates/nlos-schema/**、gen/**、crates/nlos-driver-llm-http/**、crates/nlos-wasm-sandbox/**、crates/nlos-capability/**(D5 选项 B) | W82-B(已声明) | 不变;若采第三通道,DAG 写集增补 blob_reference.proto(登记动作归 INT) |
| `schema/golden/epochs/v1.1/**`(新) | 无归属 | **建议显式并入 B 写集**(黄金语料是 schema 面的另一半) |
| `tests/stage_e_road_e001_upgrade_gate/src/step_c_in_place_upgrade.rs:387` | 不在任何 W82 车道写集;主 CI verify 作业内跑 | **建议随 B 车道显式扩写集并按 AGENTS.md 登记**(简报同建议);另注意其 :358-369 对 canonical YAML 的**字面串替换前提**:B 的 manifest 编辑保持 `current_epoch: 1` 原串、不重排 Envelope/DriverProvider 块(只在其后追加),替换逻辑即不受扰 |
| sdk/python/nlos_sdk/common.py:43、sdk/typescript/src/common.ts:42 | W82-D | **跨车道协调项**:D 在客户端交付时登记;**B-1 的六处断言松绑必须先于 D 扩表落地**(否则 D 在无归属文件里爆红);建议 B/D 在 B82-SDK 屏障对表一次(B 出 proto 定稿即冻结通道名与 (1,0) 版本对,D 照抄) |
| sdk/go/sabi/messages.go:38、sdk/csharp/llmos-sabi-probe/Messages.cs:38 | **无归属**(D 只有 python/ts) | 建议并入 B(纯机械镜像登记,与 Rust 注册表同一逻辑变更);或 INT——二选一,建议 B。改表须同组松绑 [Program.cs:592](../../sdk/csharp/llmos-sabi-probe/Program.cs) 与 [golden_test.go:508-509](../../sdk/go/sabi/golden_test.go) 的计数相等断言(两套件不在 CI 内,B-1 手工跑一次) |
| tests/conformance/schema/envelope.py:1072/:1077-1079、envelope.ts:1328-1332/:1338-1345(六处松绑,§10.2;C#/Go 计数相等随其表登记同组) | **无归属**(D 只有 tests/conformance/ipc/**) | 建议并入 B(集合/覆盖断言松绑是注册表增长的直接机械后果,且须先于 D 扩表) |
| crates/nlos-compat-shim/src/sabi_adapter.rs:847 map_service_to_channel | 无归属(nlos-compat-shim 不在任何 W82 写集) | **INT 协调项**:挂载新 service 名时把 `llm_gateway`/`tool_sandbox`/`blob_reference` 加入保留名表;B 在 proto 注释钉死 service 名供对表 |
| 根 Cargo.toml / Cargo.lock | INT 保留(§E.3.2 共享入口纪律) | **B 零依赖新增**的设计(D8 journal/std::fs、D9 手写 JSON)即为此——唯一例外是 D9 摘要原语的推荐路线(INT 前置预锁 sha2,W80-0-SCAFFOLD 同型);若实现期另发现确需新依赖,上报 INT 于屏障处处理,车道内不得自改 |

**请裁定**:通道数(11/12)与 10.3 各写集扩展归属。

## 11. D11:分阶段落地顺序与验收点

**Phase B-1 机械 schema 步**(proto ×2-3 + manifest + REGISTRY + 常量 + 函数族 + 三处 nlos-schema 测试文件改 + step_c 第 6 处 + build.rs + gen 提交 + epochs/v1.1 + SDK 表/断言松绑的 B 侧部分):
验收门 = ①`npm run schema:generate && npm run schema:check-generated` 零漂移(旧 18 字节不变、新文件入册);②`buf lint` + PR 上 `buf breaking` 绿;③`python tools/check_golden_immutability.py` 绿 + `--check-git-diff <基线>` 归档证明 epochs/v1.0 11 向量与 7 根 hex 零漂移;④手工双 YAML compat-diff JSON 报告归档(§10.2 协议,compatible + 全 ADDITIVE_SAFE);⑤`cargo test -p nlos-schema` 全绿;⑥`python -m unittest discover -s tests/conformance/schema` 全绿(松绑后);⑦canonical_deprecation_manifest() 交叉校验过(deprecation.rs:348/377 的 manifest↔registry 强一致)。

**Phase B-2 llm_gateway 服务层**(nlos-driver-llm-http 内:service 分派壳(handle_for_ipc 模式)+ D8 预扣组合 + 价目表 + D4 咨询入账 + 衰减):
验收门 = 五维预扣硬熔断单测(余量不足零发包、失败零扣减、并发窗口串行、SETTLE_REJECTED 对账行、崩溃重放后不变量三式全立——§8.3);服务端计价(无客户端 cost 字段,CJK 保守系数);record_external_usage 幂等/单调/不入 remaining + **行数上限负向**;attenuate_budget **存活子账户数上限负向**;SabiRequestContext 校验(deadline/幂等键)复用 [validate_sabi_request_context](../../crates/nlos-schema/src/lib.rs) 纪律;错误面按 StoreError→SabiFailure 映射表惯例。端到端 daemon 挂载归 W82-INT(本文只定义 service 名与方法契约)。

**Phase B-3 tool_sandbox 服务层**(nlos-wasm-sandbox 内:工具 ABI + 目录 + stdout/写入双上界 + 取消/效果建模 + 持久 ToolOperationStore):
验收门 = §9.2/9.3 全部正负向单测 + 既有 13+1 逃逸矩阵全绿(零回归)+ stdout 超限与**写入预算超限** kill 负向 + 默认 profile **epoch 非零**断言 + **daemon 中途被杀 ⇒ 孤儿在 epoch 预算内自灭**负向 + 取消三态(CancelledBeforeEffect/EffectUnknown/Completed)显式断言 + 幂等重放含结果内容、异摘要冲突、崩溃(杀宿主进程)后 journal 对账。摘要原语若走 INT 预锁路线,前置提交先于本阶段。

**Phase B-4 L-015 原语**(capability additive migrate_v3 令牌表 + BlobReference 服务层**库形态**(围栏校验 + fd 钉住 + 逐块鉴权,驻 nlos-capability——该 crate 已具备 rusqlite+sha2 依赖)+ blob_reference.proto 通道方法 + nlosref:// URI 约定文档化):
验收门 = 令牌表 DDL 保护测试(v3 类比)+ mint 围栏/摘要校验负向 + **换靶负向(mint 后以符号链接与硬链接替换文件,及逐块读之间替换——read 必须仍从钉住 fd 出且逐块摘要校验)** + read 逐块吊销/计数/TTL/**holder principal 重验**负向 + **日志外泄 URI 惰性测试**(非属主拿 URI 无法完成读取)+ 中途吊销有界泄漏断言 + **库内双主体集成演示**(发送主体落盘 WAV→mint→nlosref URI→接收主体 read 逐块还原,字节全等)。**诚实边界**:daemon 挂载与 SDK 客户端是 INT/D 交付物;qq-agent 真实退役(改其仓 `llmos_bus.py` 的 `_spill/_rehydrate_voice_uri`)在 Dogfood/INT 阶段取证,B 的库级演示是其后备证据。TCB/DoS 论证(第 12 章)随本阶段证据化(内存上界与并发读限实测数字回填)。

依赖注记:B-1 是其余各步的地基(proto 定稿冻结通道名);B-2/B-3/B-4 写集互不相交(driver crate / sandbox crate / capability+schema 追加),B-1 后可最大宽度并行(§7 最大宽度偏好);端到端(daemon 挂载、SDK 客户端、qq-agent 实景)分别归 INT / D / Dogfood。

## 12. L-015 选项 B 的 TCB 增量分析与内存/DoS 有界性安全论证(必备件)

### 12.1 威胁模型与裁定原则映射

裁定原则「依旧以绝对安全为系统设计宗旨」。威胁面:持会话能力的恶意发送方/接收方、被吊销后试图续读的持有者、试图以引用原语越出工作区围栏读任意宿主文件者、试图以铸造/读取风暴耗尽 daemon 内存或磁盘者。两条路线共同目标:大载荷不破坏 64 KiB 帧界、不破坏 epochs/v1.0 零漂移、不经通道明文透传。

### 12.2 选项 B 设计的 TCB 增量

- **零传输层改动**:nlos-ipc / nlos-system-control 一行不动——帧仍是 ≤64 KiB 的一元 exchange,无重组、无分序、无部分状态。与 W82-A 写集零交叠。
- **TCB 新增面 = 三个可审计小件**:①capability-authority.db 内 additive 令牌表族(v3,全 STRICT + 不可变触发器 + 单调吊销——与 v2 同一安全语法);②BlobReference 服务层(nlos-capability 库形态:mint 围栏校验 + **fd 钉住** + read 逐块鉴权;daemon 挂载归 INT);③D8/D9 同源的持久 journal。全部在既有权威纪律内(签名命令、单调账本、fail-closed 校验),无新密码学原语、无新信任锚。
- **字节不经通道明文**:帧内只有 (reference_id, digest, len, offset)——机密性上载荷内容不出现在任何 SABI 帧或日志;完整性由令牌内全文 SHA-256 + **逐块摘要清单**绑定,读侧每块即时可验(不依赖期末全验——期末全验是先泄后验)。
- **可衰减可吊销(与 A 的本质差)**:令牌自带 TTL/读次/块大小/持有者约束;吊销单调入表,**每块读取前校验**——传输中途吊销在下一块生效,已泄有界(§5.4 第 4 条)。选项 A 的分帧在途载荷没有任何吊销语义。

### 12.3 内存/DoS 有界性(逐向量)

| 向量 | 界 | 依据 |
|---|---|---|
| 单帧内存 | ≤64 KiB 服务载荷(外层 envelope ≤1 MiB,与 v1.0 同包络) | D2 论证;decode 前长度检查零分配 |
| 重组缓冲 | **不存在**(设计上消除) | read 是逐块独立小帧,游标在服务端令牌侧,无传输层重组态——选项 A 的每连接重组缓冲及其 stall/interleave 攻击面整体不出现 |
| mint 风暴 | 每会话铸造速率受限(per-principal 配额 + TTL 上限) | **活跃令牌**有界:≤ 每主体配额 × 平均 TTL(累计行数另见「令牌表膨胀」行,两者是不同的界,不混谈) |
| read 风暴 | 每令牌 `max_reads` 与全局 per-session 并发读上限;单块 ≤64 KiB | 并发读 × 64 KiB 即最坏驻留,配额钉死 |
| fd 钉住 | 读取一律从 mint 时钉住的 fd 出(§5.2),被钉 fd 数 ≤ 活跃令牌数 | mint 配额同时封顶 fd 持有量;release/TTL 到期即释放 |
| 磁盘耗尽 | **v1.1 零新增中心存储**:引用指向发送方自己工作区内已有文件(mint 校验存在性与摘要,不复制字节);qq-agent 场景 WAV 本就在其落盘缓存 | **诚实修正:仓内不存在工作区/磁盘配额**(燃料/epoch 之外无字节界)——发送方自己工作区的既有占用面,v1.1 不声称新增界也不恶化它;沙箱工具侧另以 `MAX_TOOL_WRITE_BYTES` 逐调用封顶(§9.2);未来若加 blob 上传存储,另立 per-principal 配额 + TTL GC(登记为演进前提) |
| 令牌表膨胀 | 吊销/过期行只增不减(审计不可变纪律,默认不清理,如实声明);过期行导出归档、主表清理的提案须新裁定 | 镜像 custody「永久审计记录」纪律;磁盘增长随时间线性——明示接受 |
| 越权读 | mint 时词法+canonicalize 双重围栏校验后**钉 fd,路径永不重解析**(换靶窗口结构性消除,§5.2);read 逐块校验 holder==解析方 principal + 块级 SHA-256 | deny-by-default;跨主体读取 fail-closed;日志外泄 URI 惰性(B-4 验收含负向) |
| URI 伪造 | `nlosref://` 仅是应用约定;服务层只认 mint 铸出的令牌,伪造 URI 解析必失败 | 令牌是不可造的 16B 派生 id + 服务端记录 |
| **网关侧两向量**(与 BlobReference 对称补齐) | `record_external_usage` 每会话每窗口追加行数上限(typed 拒绝);`attenuate_budget` 每父账户存活子账户数上限(§8.2 第 8 条) | 单会话可造成的 journal 行数与权威账户行数均有显式上界;B-2 验收含负向 |

### 12.4 与选项 A 的等强对照(裁定要求的对偶论证)

| 维度 | A 传输层分帧重组 | B 引用传递(推荐) |
|---|---|---|
| TCB 增量 | 重组状态机进 nlos-ipc/system-control(TCB 核心路径;与 W82-A 写集交叠须串行) | 旁路小件(令牌表+服务层),传输层零改动 |
| 内存界 | 每连接重组缓冲 ≤ 新全局限(必须新设总载荷上限常量,实为变相抬高帧界)+ 在途部分帧驻留(部分发送后 stall 可挂住内存 × 连接数) | 无重组态;配额 × 64 KiB 显式可算 |
| DoS 新面 | 部分帧 stall、分序 interleaving、重组计数器攻击 | mint/read 风暴(配额钉死)、令牌表膨胀(TTL) |
| 吊销语义 | 在途载荷不可吊销 | 逐块校验,下块生效 |
| 持久化/审计 | 重组缓冲是易失传输态,无审计对象 | 令牌铸造/读取/吊销全审计行 |
| 消费者复杂度 | 每个五语言 SDK 都要实现分帧重组器(×5 面) | SDK 只加 mint/read 客户端(一元调用,与现有方法同形) |
| 冻结契约 | 名义上不动 ChannelControl,但传输语义变化等效于改通道行为 | 全部既有通道字节与行为零变化 |
| 功能上限 | 上限=新总载荷常量(仍需定界,问题后移) | 上限=发送方工作区文件(天然有界),引用可跨 llm_chat/工具参数/语音多场景复用 |

结论:A 在每个安全维度都不优于 B,且在 TCB 与内存两维明确更差;维持裁定登记的默认路线 **B**。选项 C(上调帧界)维持否定:破坏 v1.0 冻结与 epochs/v1.0 零漂移门,无救济路径。

## 13. 跨车道协调项汇总(供 INT 装填)

1. **W82-A**:薄通道 service 名(`llm_gateway`/`tool_sandbox`/`blob_reference`)进入其会话分派面;D3 拒帧预算依赖其循环错误处理——**「catch 拒帧/畸形帧后继续循环(计入预算)」必须成为 W82-A 的显式验收项**(今天 daemon 是一连接一 exchange、错误即弃连,[daemon.rs:1193-1194](../../crates/nlos-system-control/src/daemon.rs),该语义尚不存在)。**现状注记**:W82-A/C/D 的 worktree 已在飞行(如 `.claude/worktrees/w82a-session-mux-*` 已有 `serve_connection` 相关提交)而台账 §E.6.1.8 仍标 `PLANNED / NOT_STARTED`——本节各写集裁定是**即时生效**的协调需求,请 INT 尽快登记并同步台账状态。
2. **W82-C**:LLMOS_ROOT_CAPABILITY_SLOT 值格式按 D7 对表;holder==principal 物化校验为其验收项。
3. **W82-D**:python/ts SABI_V1_CHANNELS 登记新通道(名与 (1,0) 以 B-1 定稿为准;**表扩之前 B-1 的六处断言松绑必须已落地**,§10.2);五语言 epochs/v1.1 解码扩展——**v1.1 读取器语义契约**:校验「manifest.required_channels == 向量覆盖集」且「required_channels ⊆ SDK 表」(权威语义见 [check_golden_immutability.py:338-345](../../tools/check_golden_immutability.py)),**不得**照抄 v1.0 读取器的活表相等/计数相等模式;客户端 LlmGatewayClient/ToolSandboxClient(+BlobReferenceClient 若采,DAG 门文本需相应增补)。
4. **W82-INT**:daemon 挂载三服务(handle_for_ipc 模式接 auth.rs 分派链或引入 service 注册表——现状是硬编码 if 链,[auth.rs:229-246](../../crates/nlos-system-control/src/auth.rs),挂载方案归 INT);compat-shim `map_service_to_channel` 增补名;DAG 写集增补(blob_reference.proto、epochs/v1.1、第 6 处断言文件与 SDK/conformance 扩展归属、INT 挂载范围与 D 客户端清单中的 blob_reference 条目);**D9 摘要原语的前置预锁提交(sha2,W80-0-SCAFFOLD 同型,若采纳推荐路线)**;规格 §E.6.1.8「~3 方法」→ 2 方法口径修正;证据文档「13 导入」勘误为 14;desktop Studio「9/9 FROZEN」徽标与 §E.6.1.8 台账状态同步。
5. **验收回执**:W82-B 行全部验收门(epochs/v1.0 零漂移 / compat-diff 纯增量 / 五维预扣硬熔断与父≥子 / 沙箱工具隔离 / L-015 TCB+DoS 论证与 qq-agent 退役)分别映射于 §11 各 Phase 与 §12。

## 14. 本文引用锚点勘误表(相对任务简报与上游文档)

- `cost_microcredits` 调用方自报的真实位置是 **nlos-driver-llm-http/src/shim.rs:258-262**(`with_cost_microcredits`;字段 :207、默认 :220),非 nlos-resource:259-263(那里是 DemandThrottle)。
- WASI 白名单实际 **14 个导入**(`add_to_linker` 注册,[wasi_allowlist.rs:412-886](../../crates/nlos-wasm-sandbox/src/wasi_allowlist.rs)),非 13(证据文档自身括号列表就是 14 个名字,属上游文档笔误)。
- **`voice_uri` 不是 ChannelControl 的 proto 字段**:它是 qq-agent 应用层 `OutboundReply` 消息的字段(E-DOGFOOD-001 §1),经 ChannelControl enqueue 的不透明 `bytes payload = 5`([channel_control.proto:88-98](../../schema/nlos/sabi/v1/channel_control.proto))传输——本文 §5.2 初稿曾误作「ChannelControl 既有 string 字段」,对抗复核已纠正;设计结论(ChannelControl 零字节改动)不变。
- 其余简报锚点(manifest:9、version_matrix:343-344、compatibility:200/:297、epoch_v1_0_golden_corpus:49、step_c:387、CI:136-139、common.py:43、shim:131-138/:612-614/:633-664、resource 六原语行号、build.rs:9-14/:48-58、buf 钉版)**全部经 9 路勘察 + 5 路对抗复核核验零漂移**;复核另修正本文自身三处:capability 触发器计数口径(heads 行由其中 2 个触发器禁改删)、`adjudicate_schema_identity` 锚点(函数在 :83)、SDK 断言松绑面(四处实为六处,§10.2)。
