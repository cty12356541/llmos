# E-DOGFOOD-001: 外部 qq-agent Dogfood 上报 L-015——ChannelControl 64 KiB 单帧上界与富媒体（语音）载荷冲突（上报受理登记）

- **状态**：`REPORTED / RULED_INTO_W82_B`（2026-11-02 上报受理登记，同日维护者裁定纳入 `W82` 并入 `W82-B` 车道，裁定原则「依旧以绝对安全为系统设计宗旨」；**实现仍零代码、零 `claims.yaml` 预支**（`AGENTS.md` 第 7 条），待 `W82` 开工执行）
- **性质**：非缺陷——`SABI v1.0` 冻结设计边界（`ADR-0014` / `W73` `L-002` 落地时冻结的 `64 KiB` 单帧上界）首次被真实外部常驻应用的富媒体场景顶到
- **日期**：2026-11-02（上报日 = 受理登记日）
- **上报方**：`qq-agent`（外部常驻双脑 Dogfood 应用，`W1 -> W4` 跨工作区架构审计移交简报渠道，接续 [W73-DOGFOOD-001](../stage-c/w73-qq-agent-dogfood-closure.md) 已闭合的 `L-001` ~ `L-007`）
- **受理侧**：`llmos` 主仓（W82 规划增补受理，见 [stage-e-progress.md](../../management/stage-e-progress.md) §E.6.1.8）
- **Base HEAD**：`cff65542`（`main`，2026-11-02 受理时点）
- **设计依据**：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §9 兼容契约 + [ADR-0014 Schema Channel Freeze v1-beta](../../management/adrs/0014-schema-channel-freeze-v1-beta.md) + [ADR-0027 Stage E SABI 长期兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) + [language-sdk-support-plan.md](../../management/language-sdk-support-plan.md)

---

## 1. 上报背景与场景

`qq-agent` 的 `send_voice` 工具将本地 F5-TTS / GPT-SoVITS（`NaiLong` 音色）合成的 WAV 以 `base64://` URI 装入 `OutboundReply.voice_uri`，经 `nlos_sdk.ChannelClient.enqueue` 走 `nlos.sabi.ChannelControl` 跨进程发送。语音 WAV 经 base64 编码后常见大于 `16 KB`，稍长语音逼近乃至超过单条 ChannelControl 帧的 `64 KiB` payload 上界，存在超限发送失败或截断风险。协议层当前没有大载荷分帧重组或旁路传递原语，帧大小约束由应用侧自行保证。

## 2. 双侧权威锚点核验（2026-11-02，llmos 受理侧亲自核验，非转述采信）

| # | 锚点 | 位置 | 核验结果 |
| :-- | :-- | :-- | :-- |
| 1 | `MAX_CHANNEL_CONTROL_PAYLOAD_BYTES: usize = 64 * 1024` | [crates/nlos-schema/src/lib.rs](../../../crates/nlos-schema/src/lib.rs) `:58` | 已核验，逐字一致 |
| 2 | 同族单帧上界（`SERVICE_DIRECTORY` / `OPERATION_CONTROL` / `SYSTEM_CONTROL` / `TAKEOVER_CONTROL` / `WAIT_CONTROL` 均为 `64 * 1024`；`PRINCIPAL_HANDSHAKE` 为 `4 KiB`、`DRIVER_PROVIDER` 为 `16 KiB`） | 同文件 `:51-58` | 已核验 |
| 3 | `ChannelControl` 冻结身份（第 9 个规范通道，`ADR-0014` 下经 `W73` `L-002` 晋升 frozen，`v1.0` 显式 `64 KiB` 上界） | 同文件 `:47-49` doc 注释 + [W73-DOGFOOD-001](../stage-c/w73-qq-agent-dogfood-closure.md) §2 `L-002` 行 | 已核验，冻结口径一致 |
| 4 | 协议层无大载荷分帧/旁路原语 | `crates/nlos-schema` 有界编解码族（`encode_bounded_with_limit` 直接以 `MAX_*_PAYLOAD_BYTES` 为硬上界 fail-closed） | 已核验（超限帧在编解码边界被拒，无重组路径） |
| 5 | 上报方台账 `L-015` 条目（日期/场景/根因/严重度/已绕过状态） | `qq-agent` 仓 `docs/evidence/dogfood-issues.md`（登记提交 `a3146a1`，在其 `main` 上） | 已核验存在且与上报一致 |
| 6 | 应用侧过渡实现（`>16KB` 落盘 `data/tts/cache`、adapter 回灌还原、非标 `localb64://` URI、用后即删） | `qq-agent` 仓 `qq_agent/llmos_bus.py` `_spill_large_voice_uri`（`:36`）/ `_rehydrate_voice_uri`（`:51`），提交 `dd4b8e2` | 已核验函数与调用点（`:528` / `:570`）存在 |
| 7 | 反夹生 W4 对照表行（过渡实现 -> llmos 接管面 -> 接管后删除动作） | `qq-agent` 仓 `docs/evidence/dogfood-issues.md` `L-015` 行 | 已核验存在 |

**未独立复现项（诚实边界）**：超限失败/截断帧未在 `llmos` 侧以最小复现测试重现；`qq-agent` 侧为联调中的风险预判加预防性落盘规避（台账状态"应用侧已绕过"）。本证据的 assurance 由此保守评定为 `POC`（观察与锚点核验，非机械复现）。

## 3. 影响面与 W82 关联

一切经 `ChannelControl` 传富媒体（语音、大图 base64）的外部应用，在 `llmos` 提供大载荷原语前都必须自建"落盘 + 回灌 + 非标 URI 约定 + 缓存清理"四件套——恰是 `W82`「让下一个外部应用删掉 `>= 60%` 系统脏活」立项要消灭的那类应用侧脏活（[stage-e-progress.md](../../management/stage-e-progress.md) §E.6.1.8 第 1 点）。`L-015` 属 `W82` 立项（`cff65542`）之后新发现的外部上报项，在原 `L-008` ~ `L-014` 拆解与 5 车道写集之外。

## 4. 接管选项（上报方建议，非结论；llmos 受理侧不加评判，供维护者裁定）

| 选项 | 内容 | 上报方标注 |
| :-- | :-- | :-- |
| A | `ChannelControl` 传输层分帧重组（`SABI v1.1` 纯增量方法，语义对应用透明） | 候选 |
| B | `Capability` 作用域 Blob/文件句柄引用传递（帧内只传引用，与 `ADR-0010` / `ADR-0022` 能力体系一致） | 候选 |
| C | 上调单帧上界 | 上报方不建议：破坏 `v1.0` 冻结与 `epochs/v1.0` golden 11 向量零漂移门禁（`W82-B` 车道验收项） |

## 5. 受理处置与裁定记录

**已完成的受理动作（2026-11-02，提交 `1a81a13f`）**：

1. 本上报受理登记证据落盘（本文件），双侧锚点核验记录如 §2；
2. [stage-e-progress.md](../../management/stage-e-progress.md) §E.6.1.8 在 `L-008` ~ `L-014` 拆解表之后增补 `L-015` 候选段（显式标注"立项后新增上报、待维护者裁定"，不改已批拆解表与 5 车道写集）；
3. [evidence-index.yaml](../../management/evidence-index.yaml) 登记本证据条目（assurance `POC`，2026-11-02）。

**维护者裁定记录（2026-11-02，同日批复「按建议纳入，依旧以绝对安全为系统设计宗旨」；裁定登记提交见 `W82.1-L015-RULING`）**：

1. `L-015` **纳入 `W82` 范围**：已裁定；
2. **并入 `W82-B-SABI-V11-THIN-WAIST` 车道**：已裁定（该车道写集随之增补 `crates/nlos-capability/**`（选项 B 路线时）与 `L-015` 验收门，见 §E.6.1.8 第 3 点 DAG 表）；
3. 技术路线 A / B：**未终裁，受裁定原则「绝对安全为系统设计宗旨」约束**——受理侧据此登记选项 B（`Capability` 作用域 Blob/文件句柄引用传递：帧内只传引用、大载荷不经通道明文透传、令牌可衰减可吊销，与 `ADR-0010` / `ADR-0022` 能力体系一致）为**默认优先路线**；选项 A（传输层分帧重组）须附等强 TCB 增量与内存/DoS 有界性安全论证方可替代（且其触及 `crates/nlos-ipc` / `crates/nlos-system-control`，与 `W82-A` 写集交叠须串行或拆分）；选项 C（上调单帧上界）维持否定。终裁在 `W82-B` 设计定稿时确认；
4. 裁定后纪律不变：`W82` 开工前仍零代码、零 `claims.yaml` 录入（`AGENTS.md` 第 7 条）。

## 6. 关联权威入口

- [ADR-0014 Schema Channel Freeze v1-beta](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)
- [ADR-0027 Stage E SABI 长期兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md)
- [W73-DOGFOOD-001：qq-agent Dogfood 全量 7 项上游问题闭环](../stage-c/w73-qq-agent-dogfood-closure.md)（`L-001` ~ `L-007`，其中 `L-002` 冻结 `ChannelControl` `64 KiB` 上界）
- [stage-e-progress.md](../../management/stage-e-progress.md) §E.6.1.8（`W82` 立项与 `L-015` 候选增补段）
- [language-sdk-support-plan.md](../../management/language-sdk-support-plan.md)（`L-008` ~ `L-014` 排期与 `W82` 关联）
- [crates/nlos-schema/src/lib.rs](../../../crates/nlos-schema/src/lib.rs)（`MAX_CHANNEL_CONTROL_PAYLOAD_BYTES` 与有界编解码族）
