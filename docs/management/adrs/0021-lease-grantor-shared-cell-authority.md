# ADR-0021：lease grantor 共享 `CellAuthority` 所有权

- 状态：`ACCEPTED`（设计裁断，2026-10-05；实现证据由后续车道落档后升 `VERIFIED`）
- 日期：2026-10-05
- Owner：nlos-lease / Cell 装配（关联 [ADR-0018](./0018-single-host-multiprocess-dual-cell-topology.md)）
- 裁断来源：W48 CellHost 装配登记的结构限制（capacity/device 族未进单进程装配）；用户授权"按推荐裁定"（2026-10-05）

## 上下文

W46 落地 lease 三族后，W48 CellHost 装配暴露：三族 grantor 各**按值**持有 `CellAuthority`，而 `CellAuthority` 的 claim slot 进程唯一且永不释放（ADR-0018）——一个进程只能存在一个 grantor 实例，导致 capacity/device 族无法与 quota 族共存于同一 CellHost。这是 API 形状产物，不是领域约束：**一个进程 = 一个 Cell**（ADR-0018），同进程内的多个 grantor 操作的是同一个 Cell 权威，属同一信任域。

## 候选

- (a) grantor 改持 `Arc<CellAuthority>`（claim 排他性由进程唯一性承载，Rust 所有权不再额外承担）
- (b) grantor 改借 `&CellAuthority`（公开 API 生命周期传染，否决）
- (c) 保持按值 + 多进程装配（把结构限制当作领域边界——与"一进程一 Cell"矛盾，否决）

## 决定

取 **(a)**：`CellAuthority` 提供 `shared(self) -> Arc<Self>`（或等价构造面）；三族 grantor 改持 `Arc<CellAuthority>`。claim 的进程排他语义不变（第二个进程 claim 仍类型化失败）；epoch 推进仍须单持有者发起（CellHost 广播链不变，见 cell-assembly-wiring-map）。API 迁移按值→Arc 为破坏性变更，但 crate 消费方当前仅 CellHost 与测试（W48 刚落），迁移窗口就是现在。

## 后果与退出

- 正面：capacity/device 族进 CellHost，七件套装配面完整。
- 负面：Arc 化弱化编译期单写者证明——用运行时不变量补（epoch 入口断言同进程唯一活动 grantor 集或以文档+测试钉住广播链单点）。
- 退出：若未来需要进程间多 grantor，本决定成为新 ADR 的输入，不静默放宽。

## 验证与复审

实现车道：三族 Arc 化 + CellHost 装配 capacity/device + 全链测试（含 epoch 广播对三族的联动）。复审触发：ADR-0018 拓扑修订（多 Cell 同进程）时重估。
