# dash — 项目推进仪表盘(Claude Code 插件)

dash 是面向 Claude Code 的轻量项目 DAG 仪表盘:会话原生(hooks 把 PostToolUse/Stop/SubagentStop 事件记入 `.dash/state.jsonl`)+ git 快照零配置(任何 git 仓库开箱即用,无需任何配置文件);当仓库存在 `.superpowers/sdd/*/dag.json` 台账时,自动以 SDD 账本为深语义适配层(波次/车道/屏障/停滞判定)。无常驻进程——每次调用即席渲染,退出即走,状态只落在仓库内 `.dash/` 与权威数据源本身。

## 快速开始

在本仓库根目录:

```bash
claude --plugin-dir plugins/dash
```

技能 `/dash` 与事件 hooks 即刻生效。会话内随时点播:

```bash
python3 plugins/dash/scripts/dash render panel   # 终端面板
python3 plugins/dash/scripts/dash oneline        # 单行摘要
```

## 安装三形态

| 形态 | 做法 | 得到 |
|---|---|---|
| 插件目录直载 | `claude --plugin-dir plugins/dash` | 全部组件:技能 + hooks + CLI(`${CLAUDE_PLUGIN_ROOT}` 可用) |
| 仓库级安装 | 把 `plugins/dash/` 整棵拷贝为目标仓库的 `.claude/skills/dash/`(含 `.claude-plugin/plugin.json`,保持插件根完整) | 技能 + CLI 随仓库走;hooks 不随 `.claude/skills/` 注册 → 无会话事件层,仪表盘退化为 git + SDD 快照;纯技能形态下 `${CLAUDE_PLUGIN_ROOT}` 不注入,请按仓库路径直接调用(如 `python3 .claude/skills/dash/scripts/dash render panel`) |
| marketplace | 待发布 | — |

## statusline 手配

主对话 statusLine 属用户/项目级 settings 配置,不能随插件分发(插件可分发的是 subagentStatusLine);要用单行摘要作状态栏,需在 `~/.claude/settings.json`(或项目 `.claude/settings.json`)手工加入:

```json
{"statusLine": {"type": "command", "command": "python3 <abs-path-to>/plugins/dash/scripts/dash oneline 2>/dev/null || true", "refreshInterval": 30}}
```

`oneline` 无 ANSI、零副作用;`2>/dev/null || true` 保证任何异常都不污染状态栏。`<abs-path-to>` 替换为仓库绝对路径。

## `.dash/` 运行时目录

会话事件(`state.jsonl`)、焦点(`focus.json`)与配置(`config.json`)都写在仓库根 `.dash/` 下,是会话本地运行时,加入 `.gitignore`:

```
.dash/
```

其中 `config.json` 是唯一人工编辑文件;若想团队共享配置,可自行改用 `!` 例外规则单独提交它。

## 配置

`.dash/config.json`(可选,缺省即用):

```json
{"stalled_threshold_h": 2.0}
```

- `stalled_threshold_h`:任务无活动多少小时判为停滞(缺省 `2.0`)。config 缺失/损坏/字段非法时自动降级缺省值,并在面板附 warning,绝不抛错。

## tmux 联动:`DASH_TMUX_TARGET`

`dash watch` 的 ⏎(送对话)与 `dash send` 通过 tmux `send-keys` 把"聚焦现场"提示送进主对话窗格。目标窗格解析顺序:`DASH_TMUX_TARGET` 环境变量 → 默认 `llmos-dag:0.0`;无 tmux 或窗格不存在时降级为可复制文本,不失败。

```bash
export DASH_TMUX_TARGET="mysession:0.0"   # 指向你的主对话窗格
python3 plugins/dash/scripts/dash watch 5  # f 聚焦 c 散焦 ⏎ 送对话 q 退出
```

## 命令一览

| 命令 | 作用 |
|---|---|
| `dash render panel` | 终端面板(ANSI,尊重焦点) |
| `dash render html` | HTML 快照 |
| `dash render mermaid [--inject]` | Mermaid 源码;`--inject` 写回最新 `.superpowers/sdd/*/progress.md`(唯一显式写副作用,且仅在 `--inject` 门后) |
| `dash oneline` | statusline 单行(无 ANSI,零副作用) |
| `dash watch [interval] [--once]` | 常驻刷新(缺省 5s);单键 f/c/⏎/q;`--once` 渲染一帧即退 |
| `dash focus <milestone\|lane\|task id>` / `dash focus clear` | 设置 / 清除面板焦点(下次面板刷新生效) |
| `dash send [--pane <target>]` | 把焦点现场提示送进主对话(tmux send-keys;`--pane` 覆盖 `DASH_TMUX_TARGET`;无焦点时报错退出 1) |

`dash`(无参数)打印用法,退出码 2。
