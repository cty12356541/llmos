# dash 插件设计——通用 Claude Code 项目推进仪表盘

- 日期:2026-09-13(brainstorming 定稿,维护者逐节确认)
- 状态:设计通过,待 writing-plans 出实现计划
- 前置依赖:`fix/dag-panel-laneless-tasks-readonly-watch`(无车道任务渲染 + `--view` 只读出口;dash 迁移时其语义原样带入,该分支应先于 dash 实现合入 main)
- 目标读者:dash 实现车道的执行者

## 1. 背景与目标

现状是一组仓库级 Claude 集成:`scripts/render_dag.py`(SDD 波次 DAG 渲染)+ `dag` 仓库技能 + statusline(`--oneline`)+ tmux 常驻面板。2026-09-13 评估结论:合格的**波次车道状态板**,但作为**项目推进仪表盘**缺四块——实时健康(卡死/异常不可见)、纵深(跨波次轨迹)、生命周期(完结波次滞留屏上)、通用性(`.superpowers/sdd/*` 路径硬编码)。

目标:演进为可推广给**任意 Claude Code 用户**的插件 `dash`(暂名),零配置起步,本仓 SDD 工作法成为其第一个深语义适配器。

## 2. 决策记录(brainstorming 定稿,2026-09-13)

| 维度 | 决定 | 被否备选 |
|---|---|---|
| 受众 | 任意 Claude Code 用户;SDD 词汇降为可选适配器 | 先 SDD 后通用(分期)——维护者选通用起步 |
| 内容主次 | ①在跑什么/健康 ②项目走到哪了 | 行动指向、信任层(Claim≤Evidence)进二期 |
| 数据源默认 | hook 捕获会话工作事件自建状态 + git 快照,零配置 | 声明式台账优先(推广门槛高);纯接口+git 单适配器(首装体验干瘪) |
| 更新机制 | 薄 hook 只写事件,面板轮询渲染(≤5s 显示延迟) | hook 直推渲染(渲染器进会话关键路径);纯轮询无 hook(丢失第一优先级内容) |
| 展示面 | 终端面板 + statusline 单行 + 会话内点播 + HTML 静态快照 | (HTML 原判 YAGNI,维护者明确要,收窄为静态单文件) |
| 架构 | 方案 A:演进现有 CLI 管道(统一模型+适配器+多目标渲染,纯 stdlib) | B:MCP 中心(面板在会话外,连 MCP 反但复杂);C:全 TUI 重写(破坏零依赖与模型-渲染分离) |
| 交互范式(2026-09-13 维护者补充) | 焦点一等状态(`.dash/focus.json`)+ 面板单键层(f/c/⏎/q)+ tmux send-keys 送主对话 + Claude 派发时主动 `dash focus` | 全 TUI 框架交互(滚动/鼠标/面板内嵌编辑)——重且绑框架 |

插件能力边界(官方文档核实,影响设计的三条硬事实):
1. 主 `statusLine` **不可**随插件分发(插件 settings.json 只认 `agent`/`subagentStatusLine`,未知键静默忽略)→ statusline 由装机方手配,README 文档化;
2. 插件不加载 CLAUDE.md 作上下文 → 本仓 §9「跃迁附图」约定的可分发形态是技能 + hooks;
3. 插件可带 `skills/`、`hooks/hooks.json`、`scripts/`(经 `${CLAUDE_PLUGIN_ROOT}` 引用)、`bin/`(进 Bash PATH)。

## 3. 总体架构:单向数据流,无守护进程

```
┌─ Claude 会话内 ──────────────┐    ┌─ 会话外(终端)──────────────┐
│ hooks.json(薄)              │    │ dash watch   面板轮询循环    │
│  PostToolUse → 追加事件      │    │ dash oneline statusline     │
│  (TodoWrite|Agent|Task|Stop) │    │ dash render html 快照       │
│ skills/dash → 会话内点播     │    └──────────┬──────────────────┘
└──────────┬───────────────────┘               │ 读
           │ 写(追加型)                         ▼
           ▼                            dash render(渲染核心)
  .dash/state.jsonl  ◄──────────────────────────┤
  (.gitignore,事件流)                          │ 读
           git 快照 ────────────────────────────┤
           SDD 台账(可选)───────────────────────┘
```

三铁律(把 2026-09-13 修掉的两个缺陷制度化):
1. **视图零写副作用**:除显式 `--inject` 注入台账,渲染路径永远只读;
2. **hook 薄且静默**:只追加事件,自身失败吞掉,绝不阻塞会话工具调用;
3. **无守护进程**:一切入口(watch/oneline/点播/HTML)都是同一个 `dash` CLI 的一次性进程,崩溃域互相隔离,多面板并存无冲突。

## 4. 统一数据模型(渲染的唯一输入)

```jsonc
{
  "project":  {"name": "llmos"},
  "milestones": [                       // 纵深:走到哪了
    {"id": "W26", "state": "done|active|planned",
     "tasks_done": 10, "tasks_total": 10, "started": "…", "ended": "…"}],
  "tasks": [                            // 任务统一五态词汇表
    {"id": "T9", "lane": "无车道", "state": "active",
     "since": "2026-09-13T14:02:00+08:00",   // 跃迁时刻,卡死检测的根
     "source": "session|git|sdd"}],
  "activity": [                         // 实时:在跑什么
    {"kind": "agent|todo|background", "label": "…",
     "since": "…", "last_event": "…"}],
  "health":   {"stalled": ["T9(active 6h)"], "failed": []},
  "velocity": {"commits_7d": 17, "milestones_done": 26}
}
```

- **五态词汇表**:`done / active / pending / blocked(依赖未满足)/ stalled(active 超阈值)`。SDD 的 fix-round、in-review 等由 sdd 适配器映射进 `active` + 注记,词汇表本身保持通用。
- **计数行口径**(面板/oneline 通用):`✓done  ▶active  ·pending+blocked  ⚑stalled`;stalled 同时计入 active,不双扣。
- **`since` 是新世界的钥匙**:上版评估的「▶5 分钟和 ▶6 小时长得一样」由它解决;stalled 阈值默认 2h,经 `.dash/config.json` 的 `stalled_threshold_h` 配置,文件缺失即全默认(零配置承诺的一部分)。
- 时间戳一律 ISO 8601 带本地时区偏移。

## 5. 适配器契约与 hook 事件

```
adapter.load(ctx) -> Fragment          # 进程内函数,非 IPC
merge(fragments) -> Model              # 冲突规则见下
```

| 适配器 | 输入 | 产出分片 | 默认 |
|---|---|---|---|
| session | 重放 `.dash/state.jsonl` | activity + task 状态(带 since);任务域=todo 快照 + agent 生命周期,属会话域,跨会话连续性由 git/sdd 分片承接 | ✓ 零配置 |
| git | 分支/提交/合并/速率快照 | milestones + velocity + 脏工作区 | ✓ 零配置 |
| sdd | `.superpowers/sdd/*`(现 `resolve_status` 逻辑整体迁入) | 任务 lane/barrier 深语义 + 屏障 + 波次生命周期 | 自动发现即启用 |

**merge 冲突规则**(Claim≤Evidence 的正向版):同一事实多源并存时,**声明源(sdd)> 事件源(session)> 派生源(git)**,深源覆盖浅源,永不反向。无 sdd 源的普通用户自然只用 session+git。

**hook 事件契约(v1 最小集,JSONL 追加)**:

```json
{"ts": "…", "kind": "todo|agent|task|stop", "event": "updated|spawned|completed|turn_end",
 "session": "…", "summary": "≤80 字符人读摘要"}
```

hooks.json:`PostToolUse` matcher `TodoWrite|Agent|Task` + `Stop`。写入脚本带 `|| true` 语义。

## 6. 渲染面:区块顺序 = 内容主次

**终端面板**(`dash watch`,窄栏 ~64-76 列优先,自上而下):

```
llmos · W26 统一恢复面                 14:57
✓10 ▶1 ·1  ⚑1 · 2 agents · 17c/7d     ← 一行版"在跑/健康"
══════════════════════════════════════
在跑 / 健康                             ← 区块 A(最高优先,人人有)
  ▶ lane-b worker · 修复环 2/5 · 42m
  ▶ agent#3 探索写集 · 5m
  ⚑ T9 全仓验证门 · active 6h(阈值 2h)  ← stalled 置顶标 ⚑
  ✓ 无失败
──────────────────────────────────────
轨迹 · 26 里程碑                        ← 区块 B(纵深,人人有)
  W24 ▓▓▓▓▓▓ 6/6 done
  W25 ▓▓▓▓▓▓▓ 7/7 done
  W26 ▓▓▓▓▓▓▓▓▓▓ 10/10 done
  下一步:待启动 W27                     ← 生命周期指向
──────────────────────────────────────
车道 / 任务(SDD 源)                    ← 区块 C(仅深语义源存在)
  semantic-ledger    worker-dual …
  ✓ T1 …             ✓ T6 …
  无车道  ✓ T9 …  ✓ T10 …
  屏障 2: T5+T7+T8 → T9 T10
```

- 区块 A/B 由 session+git 驱动,对所有用户存在;区块 C 仅在 sdd 适配器发现时出现——通用与深语义的分层在视图上同样成立。
- 每行年龄(`42m/6h`)来自 `since`;时钟=渲染时刻(新鲜度)。
- 列宽计算改用 `unicodedata.east_asian_width`(清掉按码点算宽的 CJK 漂移债,仍零依赖)。

**oneline**:`[dash] llmos W26 ✓8▶2·1 ⚑1 ·2ag`(statusline 调用,装机方手配)。
**会话内点播**:`skills/dash` 调 `dash render panel`,输出进对话;同时承载「跃迁附图」约定的可分发形态(替代 CLAUDE.md §9 对外部用户不可分发的问题)。
**HTML 快照**:`dash render html > dash.html`,单文件、内联 CSS、零外链零 JS、`prefers-color-scheme` 双主题;区块同面板。

### 6.1 交互与聚焦(2026-09-13 维护者补充定稿)

**焦点是一等状态**,不是 TUI 的局部变量:

```
dash focus T9        # 写 .dash/focus.json {target, ts},一切渲染器尊重
dash focus lane-b    # 可聚焦 milestone / lane / task
dash focus clear
```

聚焦后面板**过滤+高亮**,其余区块压缩为一行摘要:

```
llmos · W26 · ▸聚焦 T9                     c散焦 ⏎送对话  14:57
─────────────────────────────────────────────
▶ T9 全仓验证门 · active 6h ⚑ · 无车道 · 源:sdd
  最近事件:fix round 2/5 · 42m
  ── 其余:W26 ✓10 · 2 agents · 无失败 ──
```

**面板单键层**(`read -n1` 内建实现,零依赖,仅是 watch 循环的输入包装,不改变"无守护进程"原则):

| 键 | 动作 |
|---|---|
| `f` | 提示输入 id → `dash focus <id>` |
| `c` | 散焦 |
| `⏎` | 把当前焦点送进主对话 |
| `q` | 退出 |

**双向互动**:
- 面板 → 主对话(`⏎`):`tmux send-keys` 向 Claude 输入框发送由焦点预组的提示(如「聚焦 T9(全仓验证门):汇总当前障碍、最近回执与下一步建议」);无 tmux 时打印可复制文本(降级矩阵同款哲学:能力缺失不白屏)。
- 主对话 → 面板:`skills/dash` 约定 Claude 在派发/进入修复环/审查某任务时主动执行 `dash focus <id>`——§9「跃迁附图」的机械化升级:不只附图,还把面板镜头带到现场;hook 事件继续保证「Claude 正在干什么」实时可见。

## 7. 插件打包与分发

```
dash/(孵化期住本仓 plugins/dash/,拆独立仓是纯移动)
  .claude-plugin/plugin.json     # name/version/description(仅 name 必填)
  skills/dash/SKILL.md           # 点播 + 跃迁附图约定
  hooks/hooks.json               # 薄事件捕获
  scripts/dash                   # CLI:render panel|oneline|html / watch
  scripts/dashlib/               # model / merge / adapters/{session,git,sdd}
                                 # renderers/{panel,oneline,html,mermaid}
  scripts/test_dash_*.py         # unittest(零第三方依赖)
  README.md                      # 安装:--plugin-dir · 仓库级 .claude/skills · marketplace
                                 # statusline 手配一节(插件不可分发主 statusline)
```

安装三形态:开发者 `claude --plugin-dir plugins/dash`;仓库级免市场(`.claude/skills/dash/` 内含 `.claude-plugin/plugin.json`);成熟后 marketplace。脚本引用一律走 `${CLAUDE_PLUGIN_ROOT}`,不得引用插件目录外路径(插件沙箱会拒)。

## 8. 本仓迁移路径(三步,每步独立可回退)

1. **引入插件并启用薄 hooks**(旧 `dag` 面板不动,双轨观察事件流质量);
2. **切换消费面**:sdd 适配器承接 `render_dag.py` 全部语义(含无车道任务渲染与 `--view` 只读语义),`dag` 技能退役并入 `dash`,statusline 换 `dash oneline`,`dag_watch.sh`/`dag_session.sh` 换 `dash watch`/tmux 布局便利层;
3. **约定改名**:CLAUDE.md §9 改引用 `dash` 技能名,约定内容不变。

## 9. 错误处理:降级矩阵

| 故障 | 行为 |
|---|---|
| sdd 源损坏/缺失 | git+session 分片照常出面板,区块 C 处显示 `⚠ sdd 源不可用` |
| state.jsonl 损坏 | 截断至上个完整 JSON 行重放(append-only 天然可恢复) |
| git 命令失败 | 空 git 分片 + `⚠`,面板不死 |
| hook 写事件失败 | 静默吞掉,绝不影响会话工具调用 |
| 全部源失败 | 友好空态提示(沿用现 dag_watch 降级路径) |

原则:单适配器死亡永不白屏;错误可见但不喧哗。

## 10. 测试策略

- **纯函数单测**(unittest,延续 `test_render_dag.py` 模式):五态解析、merge 优先级、since/stalled 判定;
- **渲染 golden 对拍**:固定模型 fixture → 各渲染目标输出逐字节比对;含聚焦态(同一 fixture × 有焦/无焦两组 golden);
- **交互降级路径**:无 tmux 时 `⏎` 打印可复制提示而非失败;焦点文件缺失/损坏视为无焦;
- **适配器 fixture**:合成 git 仓库(`git init` + 假提交)、合成事件流、SDD 工作区样例;
- **hook e2e**:伪造 PostToolUse stdin → 事件落盘断言;
- **性能门**:10k 事件重放 + 渲染 < 100ms(v1 目标,防退化为卡顿面板)。

## 11. 范围外(YAGNI 清单,二期再议)

MCP server 形态;**TUI 框架**(textual 等——聚焦/单键/送对话已入 v1 范围,框架本身仍砍);~~行动指向(可派发集)~~——**维护者 2026-09-13 确认升级为二期确认需求**:「需要真正的调度能力」= 入度归零 + 写集锁的真调度器(per-task `after` 边 + 写集声明;merge 计算 blocked/ready;渲染点亮 ⚡可派发集;派发决策留控制器),**工具波 T2** 头号议题(2026-09-20 改号:工具链波次用 T 编号,不占台账 §6.5 的 W 车道号),先走 brainstorming 定边界;信任层(台账↔git 交叉校验、CI 状态)仍二期;多机/团队共享状态;HTML 交互与外链资源;issue/PR 数据源适配器。
