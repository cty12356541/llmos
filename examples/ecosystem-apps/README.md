# ecosystem-apps — W80-E6B 第三方多组件参考 Application 样板

Stage E `E6`(`ADR-0027` §2.4)的解耦第三方参考生态样板之一:两个独立"发布者"的多组件
Application,仅依赖公开权威 crate(starter-assistant-app 先例),零新增第三方依赖
(`ed25519-dalek` / `sha2` 沿用既有版本)。

| Crate | 形态 | 规格覆盖 |
|---|---|---|
| `knowledge-observatory-app` | 知识观测站:多角色任务 DAG `harvest→index→publish`(升级追加 `verify`),五维 LLM 预算、Artifact 血缘、SameMajor 热升级与干净卸载 | `PKG-MULTI-001` / `PKG-SEP-001` / `PKG-IMM-001` / `PKG-SEED-001` |
| `field-survey-app` | 野外巡检记录站:DAG `plan-route→collect-notes→sync-notes`(升级追加 `review-notes`),含 SameMinor 拒绝与冻结节点改写拒绝负向门 | 同上 |
| `n1-baseline-sealer` | N-1 基准封存器:`nlos-publisher-trust` 三级证书链 + `nlos-certify` 四门认证 + `nlos-store-registry` OCI 目录封存(Candidate→Stable)与封存清单落盘 | `ADR-0027` §2.4 步骤 A |

每个 App 包树声明 6 个多角色 entry(executable + background-service + driver 桥 +
UI surface 卡 + 2 个只读 seed),商店封存时映射满五类 `StoreComponentKind` 槽位。

## 封存 N-1 冻结签名基准(一条命令)

```bash
bash examples/ecosystem-apps/seal-n1-baseline.sh
```

产物:各 example `sealed/` 下的 `.nlos-pkg`(v1.0.0 + v1.1.0)、四门认证 JSON 与
store-seal 封存记录(字节确定性,重跑哈希不变)。

## 验证

```bash
cd examples/ecosystem-apps
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```
