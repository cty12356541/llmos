# ADR-0014：Schema 注册表通道冻结 v1-beta

- 状态：ACCEPTED
- 日期：2026-08-29
- Owner：nlos-schema REGISTRY / B-TYPES
- 关联 Requirement：`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`、`TYPE-GEN-001`（追踪面承 [ADR-0003](0003-stage-b-idl-and-canonical-encoding.md)）
- 关联工作包：`B-TYPES`、`B-SCHEMA`、`B-SDK-LANG-EVAL`、`B-SLICE-K`
- 决策来源：用户于 2026-08-29 决策会话在三候选（通道冻结/等齐一次冻结/全量立刻冻结）中选择「通道冻结」
- 复审触发器：Slice K 端到端贯通（触发 v1.0 晋升评估）；出现 breaking 需求；`B-SDK-LANG-EVAL` 第四语言 golden 探针（Go/C#，同日决策解封末位排队）发现冻结契约缺陷

## 上下文

`B-TYPES` 长期挂着「public schema 与生成约束未冻结」未决项。`nlos-schema` REGISTRY 现有 6 个 canonical 条目：`nlos.sabi.Envelope`（v1.1，含 B-SCHEMA-009 以 additive 加入的 common context）、`nlos.sabi.ServiceDirectory`、`nlos.sabi.OperationControl`、`nlos.sabi.SystemControl`（承载 barrier observation 与 artifact recovery contract 消息）、`nlos.sabi.TakeoverControl`、`nlos.sabi.WaitControl`（均 v1.0），全部配备三语言（Rust/TypeScript/Python）bounded codec 与 conformance golden，证据链见 [ADR-0003](0003-stage-b-idl-and-canonical-encoding.md)。

同日两项决策改变了冻结的紧迫格局。其一，Slice K 纵切面（含 SDK/CLI/打包车道）已定案立即全量并行启动，这些车道直接消费 schema，冻结策略就是它们的地基。其二，跨进程认证（签名贯穿 + AuthorityClock，[ADR-0011](0011-ipc-principal-auth-signature-passthrough.md)）必然新增 wire 面：握手 envelope、签名命令扩展、时钟类型。

[管理 README §7](../README.md) 规定 schema/durable format 变更必须走 ADR；[议题 31](../../discussions/31-重复建设评估与继续投入边界.md) 曾警告「不应推动 SABI 在 Task/Effect/Receipt 语义稳定前过早冻结」。已落地通道冻结、未落地通道保持开放，是同时满足两者的切分点。

## 候选

| 候选 | 结论 |
|---|---|
| A. 通道冻结（版本化双轨：已落地条目即时冻结为 v1-beta，未落地 auth/clock 面开放待 additive 落表） | **采纳** |
| B. 等认证/时钟落地后一次性冻结 v1.0 | 否决：已启动的 SDK/CLI/打包车道在未冻结地面施工，conformance golden 反复漂移，制造返工 |
| C. 现在全部冻结，含未落地 auth 面 | 否决：强行冻结尚不存在的设计会锁死认证决策空间，重蹈议题 31 警告 |

## 决定

1. **6 个 canonical 条目即时冻结为 v1-beta**：wire 字节锁定，此后仅允许 additive 扩列（新增 field number / 新消息类型），禁止改号、改语义、删字段；遵循 protobuf 惯例与项目既有 bounded codec additive 纪律。ADR-0003 中「common context 尚未冻结为稳定 ABI」的保留随本条一并入冻。冻结标记机械写入 REGISTRY 代码属于后续实现切片，本 ADR 只是策略定案。
2. **认证/时钟 wire 面 additive 落表**：ADR-0011 的握手 envelope、签名命令扩展、AuthorityClock 类型作为 additive 新消息加入既有 schema 命名空间，不触碰已冻结字节。
3. **golden 按冻结点钉定**：conformance golden 以本 ADR 定案日的 wire 形态为准，三语言 codec 测试继续背书逐字节一致性；此后任何 golden 变化必须可解释为 additive 扩列，否则视为 breaking。
4. **v1.0 晋升条件**：Slice K 端到端贯通后，经独立 ADR 把 v1-beta 晋升为 v1.0 正式冻结。

## 后果与退出策略

- `B-TYPES`「public schema 与生成约束未冻结」未决项关闭；SDK/CLI/打包车道获得稳定地基，生成物 drift gate 的职责从「防意外漂移」升级为「防 breaking」。
- 冻结后任何 breaking 需求须新 ADR + migration 路径 + golden 重钉，缺一即否；仅靠 CI 拒绝不构成充分响应。
- additive 扩列仍须遵守既有 bounds 与兼容规则（unknown critical fail-closed、各 payload 独立上限等），「可加」不等于「免审」。
- 退出：若冻结契约被证伪（复审触发器 3），以补记修订本 ADR 并登记 migration，不重写历史；通道划分本身若需重划（如 auth 面进一步分裂），走新 ADR。
