# 需求/风险/证据机器台账 Schema v1（FROZEN-additive）

> 版本：v1，2026-09-20 冻结（W27-E / X-4，base `3d81a90`）。来源：[管理机制 §8](./README.md#8-需求与证据台账)；字段充分性已经 Slice K 起的纵切面验证（[stage-b-progress §6.5.1 X-4](./stage-b-progress.md)）。
>
> **FROZEN-additive**：v1 字段名与语义冻结；后续只允许*新增可选字段或枚举值*（须在本文件登记并升 v1.x），不得改名、改语义、删字段。破坏性变更须升 v2 并迁移。
>
> 配套校验：`python3 scripts/lint_claims.py`（stdlib 零依赖，规则见 §4）。

## 1. 文件布局与角色

| 文件 | 内容 | 权威关系 |
|---|---|---|
| `claims.yaml` | 需求→状态→证据的 Release Claim 台账 | 只登记 [stage-b-progress §3/§4/§6](./stage-b-progress.md) 已断言的事实；全量回填等六门 Evidence 齐 |
| `risks.yaml` | 风险台账（[README §9](./README.md#9-风险管理) 九类） | 现状可稀疏，诚实优先 |
| `evidence-index.yaml` | `docs/evidence/stage-b/` 全量索引 | 与目录**双向一致**（lint 检查） |

## 2. 引用文法（三种 refs 共用）

- **repo 相对路径**：以 `/` 分隔、相对仓库根，必须真实存在（文件或目录）；
- **commit 串**：7–40 位十六进制（如 `3d81a90`）；
- v1 不支持 URL；CI run 链接仍留在 stage-b-progress / 证据文件正文内。

## 3. 字段定义

### 3.1 `claims.yaml`（顶层为条目列表）

| 字段 | 必填 | 类型/枚举 | 说明 |
|---|---|---|---|
| `requirement_id` | ✓ | str，唯一 | 需求/工作包/退出门 ID（如 `TASK-COMMIT-001`、`B-TASK-001`、`ROAD-B-003`） |
| `stage` | ✓ | `"B"`（v1 仅阶段 B） | |
| `status` | ✓ | `DONE` `PARTIAL_PASS` `IN_PROGRESS` `READY` `BLOCKED` `NOT_STARTED`（stage-b-progress §2） | `PARTIAL_PASS` 只能声称 Evidence 覆盖的局部范围 |
| `implementation_refs` | ✓ | list[str]，可空 | 路径或 commit 串 |
| `test_refs` | ✓ | list[str]，可空 | 路径或 commit 串 |
| `evidence_refs` | ✓ | list[str]，可空 | **只允许** `docs/evidence/stage-b/` 下已被索引的路径 |
| `assurance` | ✓ | `DESIGN` `H0`–`H8` | v0.5 §47 证据阶梯；`DESIGN`=仅有设计 |
| `limitations` | ✓ | list[str] | `status=PARTIAL_PASS` 时必须非空 |
| `source` | ✓ | str，非空 | 断言出处（stage-b-progress §3/§6 行 + 基线 commit），防无出处发明 claim |

### 3.2 `risks.yaml`（顶层为条目列表）

| 字段 | 必填 | 类型/枚举 | 说明 |
|---|---|---|---|
| `id` | ✓ | str，唯一（`RISK-B-NN`） | |
| `category` | ✓ | `security-bypass` `durable-state-loss` `resource-oversell` `cancel-effect-unknown` `premature-format-freeze` `runtime-ui-lockin` `scale-impersonation` `single-person-dependency` `third-party-supply-chain` | 与 [README §9](./README.md#9-风险管理) 九条一一对应 |
| `severity` | ✓ | `P0` `P1` `P2` | P0 阻止阶段退出；P1 须有缓解措施与复查点（lint v1 不强制 owner，v2 候选） |
| `status` | ✓ | `open` `partial` `mitigated` `closed` | `partial`=有已验证缓解但未闭环 |
| `description` | ✓ | str | |
| `mitigation` | | str | |
| `evidence_refs` | | list[str]，规则同 claims | 现状可稀疏 |
| `review_point` | | str | 复查点（日期或事件） |

### 3.3 `evidence-index.yaml`（顶层为条目列表）

| 字段 | 必填 | 类型/枚举 | 说明 |
|---|---|---|---|
| `id` | ✓ | str，唯一 | 取文件名 stem |
| `path` | ✓ | str，唯一 | `docs/evidence/stage-b/<file>`，必须存在 |
| `title` | ✓ | str | 证据文件 H1 |
| `scope` | ✓ | str | 一行范围（v1 取标题释义句） |
| `assurance` | ✓ | `POC` `H0`–`H8` | 该工件可支撑的**最高** claim 等级（保守评定；不可为 `DESIGN`） |
| `date` | ✓ | `"YYYY-MM-DD"` | 优先文件头声明日期；未声明取 git 首次提交日期 |

## 4. Claim≤Evidence 与 lint 规则（v1）

支撑格：`rank(POC)=3`，`rank(Hn)=n`。claim 合法当且仅当：

1. `assurance=DESIGN` ⇒ `evidence_refs` 为空；
2. `assurance≠DESIGN` ⇒ `evidence_refs` ≥ 1 条，且 `rank(assurance) ≤ max(rank(被引索引条目))`；
3. 全部 refs 可解析（路径存在/commit 形状合法；evidence_refs 须已在索引中）；
4. 枚举合法、必填字段齐、无未知字段（FROZEN）、ID 唯一；
5. 索引与 `docs/evidence/stage-b/` 目录双向一致。

lint 退出码：0=PASS，1=任何 ERROR；发现按文件逐条打印。

## 5. Canonical YAML 子集（三文件强制风格）

- 顶层为块序列（`- ` 顶格，后续键缩进 2）；仅 `key: value`、块序列、`[]` 空列表；
- 含特殊字符的字符串一律双引号（日期也引号化，保证与 pyyaml 解析同型）；枚举裸 token 不加引号；
- 无锚点、别名、多行标量、行尾注释；整行 `#` 注释允许；
- `lint_claims.py` 内置 stdlib 子集解析器（仓库测试零依赖约定），装有 pyyaml 时测试额外验证两者解析等价。
