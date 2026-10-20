# ADR-0022：capability 签发闭环——operator 密钥文件路径

- 状态：`ACCEPTED`（设计裁断，2026-10-05；实现证据由后续车道落档后升 `VERIFIED`）
- 日期：2026-10-05
- Owner：nlos-capability / nlos-system-control daemon
- 裁断来源：2026-09-25 两轮审查登记"capability 签发闭环仍零生产调用"；2026-09-29 会话分析确认根签发无天花板（议题 10 静态核留机器外）；用户授权"按推荐裁定"

## 上下文

`nlos-capability` 的 `issue_root_signed` 是签名门控的（ADR-0010），但**没有任何生产调用方**：系统能装配 CapabilityAuthority（W40-D 起 slice-k 已装配）却无人签发第一张根能力。议题 10 的 L1/L2（Policy 守护进程自动签、人类上行）均未实现；组织信封模板停留在部署策略。与此同时 W43-E2 已给 semantic durability_receipts 落了生产签发方——capability 侧对等缺口仍在。

## 候选

- (a) operator 密钥文件路径：daemon 启动时按 operator 提供的 Ed25519 密钥文件签发根能力（密钥 0600、不入库、与 desktop principal key 同惯例）
- (b) 完整 L1 Policy 守护进程（规则引擎自动签发）——议题 10 全量，工程量大且依赖规则模型设计
- (c) 内嵌开发密钥——违反"密钥不入库"惯例，否决

## 决定

取 **(a)** 作为闭环最小面：`nlos-system-control` daemon 新增启动期 operator 根签发步——operator 密钥文件路径经配置显式提供（缺省**不签发**，capability 面保持关闭=fail-closed）；签发幂等（同密钥+同参数重放原回执）；根能力绑定 operator principal、purpose 摘要可选、有效期与再委托深度取保守缺省。组织信封模板（把根上权利约束为部署声明的子集）登记为后续增强，不在本 ADR 范围。L1/L2 维持议题 10 未实现状态。

## 后果与退出

- 正面：签发闭环打通，semantic 准入链（authorize_semantic/consume）首次有生产供给；密钥治理沿用既有惯例零新概念。
- 负面：operator 密钥即天花板——持有有效密钥者可签任意根（与现状同），模板约束后置。
- 退出：L1 Policy 守护进程落地时，operator 直签收窄为模板内自动签；不废除密钥路径（作为 L1 的兜底人工通道）。

## 验证与复审

实现车道：daemon 启动签发步 + 幂等/缺省关闭/密钥文件权限测试。复审触发：议题 10 L1/L2 立项时。
