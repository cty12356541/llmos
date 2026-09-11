# NLOS (llmos) 项目约定 —— Git 工作法速查

> 本文件是操作层速查;规范权威仍以 `AGENTS.md`(11 条最高规则)与
> `docs/management/project-knowledge-progressive-disclosure.md`(§6.1 子 Agent 编排、§8 提交协议)为准。
> 冲突时:最高目标 > v0.5 规范 > ADR > 进度单 > 代码 > 本文件。

## 1. 身份与配置

- 提交作者一律使用:`cty12356541 <171764500+cty12356541@users.noreply.github.com>`
- 仓库级配置即可,勿改全局;不带单位/机器名等占位身份
- 外部协作产生的提交须由维护者确认作者归属后再进 canonical

## 2. 分支模型

- `main` = 唯一 canonical 分支,保持随时可发布
- 功能/修复走独立分支,命名:`fix/<scope>-<slug>` 或 `feat/<scope>-<slug>`(scope 用 crate 名或主题,如 `fix/windows-portability-sync-dir-and-uri-guard`)
- `dependabot/*` 分支只由 dependabot 维护,人工不碰
- 禁止 force-push、禁止 rebase 改写已推送的提交(AGENTS.md 规则 10)
- 提交前检查 HEAD 漂移:若基点落后于远程,先更新再提交

## 3. 提交信息约定(从 495 次提交历史提炼)

格式:``<type>(<scope>): <subject>``(subject 用中文,祈使句,不加句号)

| type | 用途 | 实例(真实历史) |
|---|---|---|
| `feat` | 新功能/新机制 | `feat(nlos-resource): add multi-dimension reservation demand prefix (W22-R)` |
| `fix` | 缺陷修复 | `fix(nlos-task): align resource fixtures with multi-dim demand fields (W22-R)` |
| `docs` | 文档/账本登记 | `docs: 登记波次 22 六车道 + §3/§6 同步` |
| `test` | 测试补充 | `test(nlos-identity): cover signing-key rotation kill-window fault matrix` |
| `chore` | 工程/初始化 | `chore: 项目初始化——gitignore 与 ui-ux-pro-max 技能` |
| `site` | 网站内容 | `site: 架构设计可视化网站(v0.2 分页版)` |

- scope = crate 名(`nlos-xxx`)或 `docs`/`site`;无明确 scope 时可省略括号
- 波次/车道编号写在 subject 尾部括号(如 `(W22-004)`、`(W22-R)`),便于回溯编排
- 正文(可选)写:动机、验证数据(跑了什么命令、结果数字)、关联 lane/Attempt 名
- 多行正文用 heredoc 撰写,保持可读

## 4. 原子提交纪律(AGENTS.md 规则 9/10 的操作化)

1. **一个 Task/Attempt = 一个可解释提交**;候选、失败或未过验收的 Attempt 不得以完成状态提交
2. **只暂存当前任务的写集**:`git add <精确文件列表>`,禁止在共享脏工作区用 `git add -A`/`git add .`
3. **提交前五查**:
   - `git log origin/main..HEAD` — 确认没有夹带他人/他任务提交
   - `git diff --staged` — 逐行过一遍将要提交的内容
   - 敏感信息扫描(令牌、密钥、绝对路径、临时文件)
   - `cargo fmt --check` 通过
   - 相关 crate 的测试通过(最小范围 `-p <crate> --test <suite>`)
4. **禁止**擅自 amend/rebase/force-push/reset 改写他人提交;需要修自己的最后一个未推送提交时可 amend,推送后一律新提交
5. push、发布、部署必须**逐级如实报告**结果;不得把本地 commit 冒充远程已发布

## 5. 验证门(按影响面递增)

| 改动范围 | 提交前必跑 | 合并前必跑 |
|---|---|---|
| 单 crate 逻辑 | `-p <crate>` 相关测试 | `cargo test --workspace --no-fail-fast` |
| 跨 crate / 存储层 | 受影响 crate 全部测试 | 全量测试 + `cargo clippy --workspace --all-targets -- -D warnings` |
| 平台相关(Windows/Unix 专属代码) | 本平台测试 + 明确声明验证平台 | 三平台 CI 全绿 |
| schema/proto | `npm run schema:generate && npm run schema:check-generated`(生成物零漂移) | conformance(TS+Py) |

Windows 专项注意(本机实测教训):
- 目录 fsync 句柄必须带写权限(`blob.rs` 的 `sync_dir`,PR #17)
- 权威 `open` 的 `create_dir_all` 必须带 `file:` URI 守卫(f81f6d2 模式)
- 已知既有失败:无(曾存在 pid_map dead_code 挂 Windows clippy,7c08586/W23-001 已修复)

## 6. 推送与 PR

- push 前确认:分支名、提交作者、无夹带;首次推送带 `-u`
- PR 标题沿用提交信息风格;正文须含:问题、根因、修复、验证数据表、未运行项显式标注(Claim ≤ Evidence)
- CI("Rust cross-platform verification")绿是合并前置;夜间 job(scale-probe/MSRV)失败单独评估
- PR 合并后:删除远端功能分支,本地分支同步清理

## 7. 与编排协议的衔接(§6.1 / §8)

- 多步实现/验证委派子 Agent 时,每车道独立 Task/Attempt + 声明写集;写集不相交才并行
- 波次收尾由单一 integrator 登记(`docs: 登记波次 N ...` 模式),避免多车道并发写进度账本
- 每条车道产出回执(实际命令+结果+未运行项),回执真实性优先于结果好看

## 8. 计划推进节奏:只按波次,不按日历

- 一切推进计划/路线图只按波次编号组织(延续 main 上"波次 N"惯例),不出现"今天/本周"等时间刻度
- 波次完成 = 全部车道过验收门,与日期无关;完成即进下一波次
- 波次内的车道依赖规则:写集不相交可并行,同文件必须串行(如 effect.rs)
