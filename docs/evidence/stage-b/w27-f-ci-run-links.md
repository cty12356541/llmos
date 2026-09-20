# W27-F：W26 CI/Pages run 链补登勘察（B3-5 前半）

> 状态：勘察记录（FACT，只读采集；`stage-b-progress.md` 由控制器统一集成）
>
> 日期：2026-09-20（W27-F）
>
> 对象：W26（`B-TASK-008C2G-UNIFIED-RECOVERY`，commits `e5203c7`..`2b51d28`，worktree 分支 `feat/unified-recovery-plane`，基线 `1a14ae0`）合入 origin/main 时登记的「CI 待 push 后触发」——本文件用 `gh` 只读列出真实 run 链，供 §3/§5/§6 对应行回填。**未 push、未触发任何 workflow。**

## 1. W26 代码如何到达 origin/main

- PR #21（`feat/unified-recovery-plane` → `main`），merge commit **`1b4628a`**（2026-09-13 14:26:45 +0800），紧随其后的 docs/chore push（`0298006`、`876cc00`、`0761f0b`）在数分钟内连续到达 main；`cee684a` 在其之后。
- 分支尖端 `590ea66`（= W26 代码 `2b51d28` + W26-004 集成收尾 docs 提交 `9399af7`/`590ea66`）在合并前有完整 pull_request CI run。
- **不存在** head SHA 为 `2b51d28` 或 `e5203c7` 的任何 workflow run（W26 实现提交未单独以 push 事件出现在任何 run head 上）——如实登记，非遗漏。

## 2. Run 清单（`gh run list`/`gh run view` 只读采集，2026-09-20）

| # | Workflow | Run ID | URL | 事件 / head SHA | Conclusion | 覆盖内容 |
|---|---|---|---|---|---|---|
| 1 | Rust cross-platform verification（三平台 + MSRV） | 34741329313 | https://github.com/cty12356541/llmos/actions/runs/34741329313 | pull_request / `590ea66`（feat/unified-recovery-plane） | **success**（ubuntu-latest / windows-latest / macos-latest / MSRV 1.97 全 success；Scale probe skipped） | **W26 代码全量**（含 `e5203c7`..`2b51d28` 全部实现提交）的三平台 + MSRV 验证 |
| 2 | Deploy to GitHub Pages | 34742762023 | https://github.com/cty12356541/llmos/actions/runs/34742762023 | push / `1b4628a`（main，PR #21 merge commit） | **success**（deploy success） | W26 合入 main 的 Pages 部署 |
| 3 | Rust cross-platform verification | 34742762041 | https://github.com/cty12356541/llmos/actions/runs/34742762041 | push / `1b4628a`（main） | **cancelled**（MSRV 1.97 success；ubuntu/windows/macos 三平台 cancelled——被数分钟内紧随的 main push 取代，非测试失败） | merge commit 上的 push CI；如回填须如实标注 cancelled |
| 4 | Rust cross-platform verification（合入后 main 上首个完整跑绿） | 34743341209 | https://github.com/cty12356541/llmos/actions/runs/34743341209 | push / `0761f0b`（= `1b4628a` + 纯 docs/chore 提交） | **success**（三平台 + MSRV 全 success） | 代码树包含 W26 全量变更的 main push CI 完整通过记录 |
| 5 | Deploy to GitHub Pages | 34743341205 | https://github.com/cty12356541/llmos/actions/runs/34743341205 | push / `0761f0b` | **success** | 同上树的 Pages 部署 |

无 Schema fuzz smoke run 覆盖 W26 变更（该 workflow 为 pull_request/特定路径触发，W26 周期未触发，如实登记）。

## 3. 回填映射建议（控制器集成时对号）

`stage-b-progress.md` 中登记「CI 待 push 后触发」的位置与建议挂链：

| 进度单位位置 | 内容 | 建议挂链 |
|---|---|---|
| 检查点行（「最后更新（当前检查点）：2026-09-13」） | `2b51d28` 基线全量门 + CI 待 push | run #1（PR 全量）+ run #2（Pages）为主；#3 如实 cancelled 或省略 |
| §5 W26 增量三行（W26-001/002/003） | 本地定向门 + 全量门 | run #1（覆盖三车道全部代码） |
| §6 ROAD-B-003 行「W26 统一恢复面……（单机 H3；CI 待 push 后触发）」 | W26 段落 | run #1 + run #2；如需 main-push 侧完整绿证加 run #4/#5 |
| §6.5 B3-5 行 | 「W26 CI/Pages run 补登」前半 | 本文件 |

**勘误说明**：W27-F 派发描述称「§3 rows」；经逐行核对，§3「当前工作包总览」（L233–L297）**不存在** W26/`UNIFIED-RECOVERY` 专属行（最接近的 `B-TASK` 行未登记 W26）。W26 的登记实际位于检查点行 + §5 + §6 + §6.5，如上表；请控制器按实际行位集成。

## 4. 附带观察（非本车道写集，供控制器知悉）

- `0761f0b` 之后的 **scheduled**（nightly）Rust CI 自 2026-09-13 21:10 起连续 failure（如 run 34783031057、34902713614……35469517760，head 均为 `0761f0b`）；`cee684a` push run 35505277162 success，但其后 `83ab8fd`..`41f4fcb` push run 多为 cancelled（连续 push 取代），**`3d81a90`（W27 基线）push run 35506588229 failure（macos-latest）**。这些晚于 W26 合入，不改变 W26 run 事实，但 W27 收官前建议另开勘察（属 B3-5「新增切片三平台复验」同族）。
