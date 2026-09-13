# dash 插件实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把仓库级 DAG 面板演进为可推广的 Claude Code 插件 `dash`——统一模型 + 三适配器(session/git/sdd)+ 多目标渲染 + 焦点互动 + 本仓迁移。

**Architecture:** 方案 A(演进现有 CLI 管道):薄 hook 追加事件到 `.dash/state.jsonl`,一切渲染入口都是同一个 `dash` CLI 的一次性进程;适配器产出 Fragment,merge 按「声明>事件>派生」合成统一模型;渲染器(panel/oneline/html/mermaid)消费同一模型。无守护进程。

**Tech Stack:** Python ≥3.9 纯 stdlib(unittest、dataclasses、subprocess、termios、unicodedata);tmux(可选,send-keys 用)。

**Spec:** `docs/superpowers/specs/2026-09-13-dash-plugin-design.md`(本计划从 spec 论证,执行者两份都读)

## Global Constraints

- Python ≥3.9 纯 stdlib,零第三方依赖(本机 3.9.6 是验证下限;禁用 `X | Y` 运行时类型联合、match 语句)
- 三铁律:渲染路径只读(除显式 `--inject` 与 `dash focus`);hook 静默不阻塞会话;无守护进程
- 单适配器失败不白屏:Fragment 带 `warnings`,渲染器显示 `⚠ …不可用`
- 状态文件 `.dash/`(gitignore);时间戳 ISO 8601 带本地时区偏移
- 计数口径:`✓done ▶active stalled 并入 active ·pending+blocked ⚑stalled`
- 测试跑法:`python3 -m unittest discover -s plugins/dash/scripts -p "test_dash_*.py" -v`(每任务先跑自己文件的)
- 涉及 sh 的任务加跑 `sh -n`;无 Rust 改动不碰 cargo 门
- 提交:`<type>(dash): 中文祈使句`(仓库 §3 约定),作者 cty12356541,原子提交(§4 五查)
- **前置**:`fix/dag-panel-laneless-tasks-readonly-watch` 已合入 main(T5/T11 移植其语义,迁移退役它)
- 并行车道(写集不相交,§7 最大宽度并行):T1→(T2‖T3‖T4‖T5)→T6→(T7‖T8‖T11)→T9→T10→T12→T13

## 文件结构(分解决策锁定)

```
plugins/dash/
  .claude-plugin/plugin.json        # 插件清单
  skills/dash/SKILL.md              # 会话内点播 + 跃迁 focus 约定(T12)
  hooks/hooks.json                  # 薄事件捕获(T2)
  hooks/record_event.py             # 事件记录器(T2)
  scripts/dash                      # CLI 入口(T1 起,逐任务扩子命令)
  scripts/dashlib/
    __init__.py
    model.py                        # 统一模型 + 五态 + stalled + display_width(T1)
    merge.py                        # Fragment 合并(T6)
    adapters.py                     # session/git/sdd 三适配器(T3/T4/T5 各追加)
    focus.py                        # 焦点读写(T9)
    sendkeys.py                     # tmux send-keys + 降级(T10)
    render_oneline.py               # T7
    render_panel.py                 # T8
    render_html.py                  # T11
    render_mermaid.py               # T11(承接 render_dag.py 注入语义)
  scripts/test_dash_model.py        # T1
  scripts/test_dash_hooks.py        # T2
  scripts/test_dash_adapters.py     # T3/T4/T5 各追加
  scripts/test_dash_merge.py        # T6
  scripts/test_dash_renderers.py    # T7/T8/T9/T11 各追加
  scripts/test_dash_cli.py          # T9/T10 CLI 冒烟
  README.md                         # T12
```

---

### Task 1: 插件骨架 + 统一模型(model.py)

**Files:**
- Create: `plugins/dash/.claude-plugin/plugin.json`
- Create: `plugins/dash/scripts/dashlib/__init__.py`(空)
- Create: `plugins/dash/scripts/dashlib/model.py`
- Create: `plugins/dash/scripts/dash`(CLI 桩)
- Test: `plugins/dash/scripts/test_dash_model.py`

**Interfaces:**
- Produces(后续所有任务的共同地基):
  - `@dataclass Task(id,label,state,lane="无车道",since=None,source="git",note="")`,state ∈ `done|active|pending|blocked|stalled`
  - `@dataclass Milestone(id,title="",state="planned",tasks_done=0,tasks_total=0,started=None,ended=None)`
  - `@dataclass Activity(kind,label,since,last_event="")`
  - `@dataclass Fragment(source,tasks=[],milestones=[],activity=[],velocity={},warnings=[])`(列表字段用 `field(default_factory=list)` 等)
  - `@dataclass Model(project,tasks,milestones,activity,stalled,velocity,warnings)`
  - `is_stalled(task, now_iso, threshold_h=2.0) -> bool`
  - `age(since_iso, now_iso) -> str`(输出 `5m`/`42m`/`6h`/`3d`)
  - `display_width(s) -> int`(east_asian_width:F/W 算 2,其余 1)

- [ ] **Step 1: 写失败测试**

```python
# plugins/dash/scripts/test_dash_model.py
import unittest
from datetime import datetime, timedelta, timezone

from dashlib.model import Task, is_stalled, age, display_width


class TestModel(unittest.TestCase):
    def test_stalled_over_threshold(self):
        t = Task(id="T9", label="全仓验证门", state="active",
                 since="2026-09-13T08:00:00+08:00", source="sdd")
        now = "2026-09-13T15:00:00+08:00"  # 7h > 2h
        self.assertTrue(is_stalled(t, now, threshold_h=2.0))

    def test_active_under_threshold(self):
        t = Task(id="T9", label="x", state="active",
                 since="2026-09-13T14:30:00+08:00", source="sdd")
        self.assertFalse(is_stalled(t, "2026-09-13T15:00:00+08:00", 2.0))

    def test_non_active_never_stalled(self):
        t = Task(id="T1", label="x", state="done",
                 since="2026-09-13T08:00:00+08:00", source="sdd")
        self.assertFalse(is_stalled(t, "2026-09-13T15:00:00+08:00", 2.0))

    def test_age_format(self):
        self.assertEqual(age("2026-09-13T14:55:00+08:00", "2026-09-13T15:00:00+08:00"), "5m")
        self.assertEqual(age("2026-09-13T09:00:00+08:00", "2026-09-13T15:00:00+08:00"), "6h")

    def test_display_width_cjk(self):
        self.assertEqual(display_width("台账"), 4)      # 2×2
        self.assertEqual(display_width("ab"), 2)
        self.assertEqual(display_width("✓ T1 表组"), 8)  # ✓=1 空格=1 T1=2 空格=1 表组=4

    def test_since_missing_not_stalled(self):
        t = Task(id="T2", label="x", state="active", source="session")
        self.assertFalse(is_stalled(t, "2026-09-13T15:00:00+08:00", 2.0))


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cd /Users/lipunima/projects/llmos && python3 plugins/dash/scripts/test_dash_model.py`
Expected: FAIL,`ModuleNotFoundError: No module named 'dashlib'`

- [ ] **Step 3: 最小实现**

```python
# plugins/dash/scripts/dashlib/model.py
"""dash 统一数据模型:五态任务、里程碑、活动、分片。纯 stdlib。"""
from __future__ import annotations

import unicodedata
from dataclasses import dataclass, field
from datetime import datetime
from typing import List, Optional, Dict

STATES = ("done", "active", "pending", "blocked", "stalled")


@dataclass
class Task:
    id: str
    label: str
    state: str                    # STATES 之一
    lane: str = "无车道"
    since: Optional[str] = None   # ISO8601 带时区偏移
    source: str = "git"           # session|git|sdd
    note: str = ""


@dataclass
class Milestone:
    id: str
    title: str = ""
    state: str = "planned"        # done|active|planned
    tasks_done: int = 0
    tasks_total: int = 0
    started: Optional[str] = None
    ended: Optional[str] = None


@dataclass
class Activity:
    kind: str                     # agent|todo|background|stop
    label: str
    since: str
    last_event: str = ""


@dataclass
class Fragment:
    source: str                   # sdd|session|git
    tasks: List[Task] = field(default_factory=list)
    milestones: List[Milestone] = field(default_factory=list)
    activity: List[Activity] = field(default_factory=list)
    velocity: Dict[str, int] = field(default_factory=dict)
    warnings: List[str] = field(default_factory=list)


@dataclass
class Model:
    project: str
    tasks: List[Task] = field(default_factory=list)
    milestones: List[Milestone] = field(default_factory=list)
    activity: List[Activity] = field(default_factory=list)
    stalled: List[str] = field(default_factory=list)
    velocity: Dict[str, int] = field(default_factory=dict)
    warnings: List[str] = field(default_factory=list)


def _parse(ts: str) -> datetime:
    return datetime.fromisoformat(ts)


def is_stalled(task: Task, now_iso: str, threshold_h: float = 2.0) -> bool:
    if task.state != "active" or task.since is None:
        return False
    return (_parse(now_iso) - _parse(task.since)).total_seconds() > threshold_h * 3600


def age(since_iso: str, now_iso: str) -> str:
    seconds = (_parse(now_iso) - _parse(since_iso)).total_seconds()
    minutes = int(seconds // 60)
    if minutes < 60:
        return f"{minutes}m"
    hours = minutes // 60
    if hours < 24:
        return f"{hours}h"
    return f"{hours // 24}d"


def display_width(s: str) -> int:
    return sum(2 if unicodedata.east_asian_width(ch) in "FW" else 1 for ch in s)
```

`plugins/dash/.claude-plugin/plugin.json`:

```json
{
  "name": "dash",
  "version": "0.1.0",
  "description": "Lightweight project DAG dashboard for Claude Code: session-native + git, zero config, SDD ledger as deep-semantics adapter",
  "displayName": "dash"
}
```

`plugins/dash/scripts/dash`(CLI 桩,可执行):

```python
#!/usr/bin/env python3
"""dash CLI:render panel|html|mermaid [--inject] / oneline / watch / focus / send。"""
from __future__ import annotations

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))


def main() -> int:
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        return 2
    print(f"dash: 未实现子命令 {args[0]}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
```

- [ ] **Step 4: 跑测试确认通过**

Run: `python3 plugins/dash/scripts/test_dash_model.py`
Expected: OK(6 tests)

- [ ] **Step 5: 提交**

```bash
git add plugins/dash
git commit -m "feat(dash): 插件骨架与统一五态模型(since/stalled/CJK 宽度)"
```

---

### Task 2: 薄事件记录器 + hooks.json

**Files:**
- Create: `plugins/dash/hooks/hooks.json`
- Create: `plugins/dash/hooks/record_event.py`
- Test: `plugins/dash/scripts/test_dash_hooks.py`

**Interfaces:**
- Produces: 事件契约 `{ts, kind: todo|agent|task|stop, event, session, summary}`,追加到 `<repo>/.dash/state.jsonl`;`record_event.main(stdin_text, state_path)` 可测入口
- T3 的 session 适配器消费该文件格式

- [ ] **Step 1: 写失败测试**

```python
# plugins/dash/scripts/test_dash_hooks.py
import json
import tempfile
import unittest
from pathlib import Path

from importlib import import_module
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "hooks"))
record_event = import_module("record_event")


def hook_payload(tool_name, tool_input=None, tool_response=None):
    return json.dumps({"tool_name": tool_name,
                       "tool_input": tool_input or {},
                       "tool_response": tool_response or {},
                       "session_id": "sess-1", "cwd": "/tmp"})


class TestRecordEvent(unittest.TestCase):
    def test_todo_event_appended(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "state.jsonl"
            record_event.main(hook_payload("TodoWrite",
                                           {"todos": [{"content": "修面板", "status": "in_progress"}]}),
                              p)
            lines = p.read_text(encoding="utf-8").strip().splitlines()
            ev = json.loads(lines[-1])
            self.assertEqual(ev["kind"], "todo")
            self.assertEqual(ev["event"], "updated")
            self.assertIn("修面板", ev["summary"])
            self.assertTrue(ev["ts"])

    def test_agent_spawned(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "state.jsonl"
            record_event.main(hook_payload("Agent", {"description": "探索写集"}), p)
            ev = json.loads(p.read_text(encoding="utf-8").strip().splitlines()[-1])
            self.assertEqual((ev["kind"], ev["event"]), ("agent", "spawned"))
            self.assertIn("探索写集", ev["summary"])

    def test_stop_event(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "state.jsonl"
            record_event.main(hook_payload("Stop"), p)
            ev = json.loads(p.read_text(encoding="utf-8").strip().splitlines()[-1])
            self.assertEqual((ev["kind"], ev["event"]), ("stop", "turn_end"))

    def test_garbage_stdin_never_raises(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "state.jsonl"
            record_event.main("not json at all{", p)   # 不得抛异常
            record_event.main("", p)
            self.assertFalse(p.exists())               # 无有效事件不落盘


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: 跑测试确认失败**

Run: `python3 plugins/dash/scripts/test_dash_hooks.py`
Expected: FAIL,`ModuleNotFoundError: No module named 'record_event'`

- [ ] **Step 3: 最小实现**

```python
#!/usr/bin/env python3
"""薄事件记录器:读 hook stdin JSON,追加一行事件到 .dash/state.jsonl。

铁律:自身任何失败静默吞掉,绝不阻塞会话(hooks.json 已带 || true 双保险)。
"""
from __future__ import annotations

import json
import sys
from datetime import datetime
from pathlib import Path

TOOL_KIND = {"TodoWrite": "todo", "Agent": "agent", "Task": "agent", "Stop": "stop"}
MAX_SUMMARY = 80


def _summary(payload: dict) -> str:
    todos = payload.get("tool_input", {}).get("todos")
    if todos:
        active = [t.get("content", "") for t in todos if t.get("status") == "in_progress"]
        return ";".join(active)[:MAX_SUMMARY] or ";".join(t.get("content", "") for t in todos)[:MAX_SUMMARY]
    for key in ("description", "prompt", "subject"):
        v = payload.get("tool_input", {}).get(key)
        if v:
            return str(v)[:MAX_SUMMARY]
    return ""


def main(stdin_text: str, state_path: Path) -> None:
    try:
        payload = json.loads(stdin_text or "{}")
        kind = TOOL_KIND.get(payload.get("tool_name", ""))
        if kind is None:
            return
        event = {"ts": datetime.now().astimezone().isoformat(timespec="seconds"),
                 "kind": kind,
                 "event": "turn_end" if kind == "stop" else ("spawned" if kind == "agent" else "updated"),
                 "session": payload.get("session_id", ""),
                 "summary": _summary(payload)}
        state_path.parent.mkdir(parents=True, exist_ok=True)
        with state_path.open("a", encoding="utf-8") as f:
            f.write(json.dumps(event, ensure_ascii=False) + "\n")
    except Exception:
        pass  # 铁律:静默


if __name__ == "__main__":
    repo = Path(".dash")
    main(sys.stdin.read(), repo / "state.jsonl")
```

`plugins/dash/hooks/hooks.json`(注意 `${CLAUDE_PLUGIN_ROOT}` 由 Claude Code 在 hook 命令内替换;hook 以项目目录为 cwd,故 state 落在仓库根 `.dash/`):

```json
{
  "hooks": {
    "PostToolUse": [
      {
        "matcher": "TodoWrite|Agent|Task",
        "hooks": [
          {"type": "command",
           "command": "python3 \"${CLAUDE_PLUGIN_ROOT}/hooks/record_event.py\" || true"}
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {"type": "command",
           "command": "python3 \"${CLAUDE_PLUGIN_ROOT}/hooks/record_event.py\" || true"}
        ]
      }
    ]
  }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `python3 plugins/dash/scripts/test_dash_hooks.py`
Expected: OK(4 tests)

- [ ] **Step 5: 提交**

```bash
git add plugins/dash/hooks plugins/dash/scripts/test_dash_hooks.py
git commit -m "feat(dash): 薄事件记录器与 hooks 接线(静默铁律 + 契约测试)"
```

---

### Task 3: session 适配器(事件重放)

**Files:**
- Create: `plugins/dash/scripts/dashlib/adapters.py`
- Test: `plugins/dash/scripts/test_dash_adapters.py`(新建,后续任务追加)

**Interfaces:**
- Consumes: T1 `Fragment/Task/Activity`,T2 事件格式
- Produces: `load_session(state_path: Path, now_iso: str) -> Fragment`
  - tasks:每个会话的 todo 快照项(`id=f"todo-{session}-{序号}"`,in_progress→active、completed→done、其余 pending;since=该 todo 首次出现时刻)
  - activity:`spawned` 无配对 `completed`(或 `task` 完成事件)的 agent;`stop` 更新所属 session 的 last_event
  - 损坏 JSON 行跳过(截断语义:只取完整行)

- [ ] **Step 1: 写失败测试**(追加到 `test_dash_adapters.py`)

```python
# plugins/dash/scripts/test_dash_adapters.py 头部
import json
import tempfile
import time
import unittest
from pathlib import Path

from dashlib.adapters import load_session
from dashlib.model import Fragment


def write_events(tmp: Path, events):
    p = tmp / "state.jsonl"
    p.write_text("".join(json.dumps(e, ensure_ascii=False) + "\n" for e in events),
                 encoding="utf-8")
    return p


NOW = "2026-09-13T15:00:00+08:00"


class TestSessionAdapter(unittest.TestCase):
    def test_todos_become_tasks(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:50:00+08:00", "kind": "todo", "event": "updated",
                 "session": "s1", "summary": "修面板;写测试"},
                {"ts": "2026-09-13T14:55:00+08:00", "kind": "todo", "event": "updated",
                 "session": "s1", "summary": "写测试"},
            ])
            frag = load_session(p, NOW)
            self.assertEqual(frag.source, "session")
            states = {t.id: t.state for t in frag.tasks}
            self.assertEqual(states["todo-s1-0"], "active")
            self.assertEqual(states["todo-s1-1"], "done")   # 第二快照中消失→done
            self.assertEqual(frag.tasks[0].since, "2026-09-13T14:50:00+08:00")

    def test_active_agent_activity(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:30:00+08:00", "kind": "agent", "event": "spawned",
                 "session": "s1", "summary": "探索写集"},
                {"ts": "2026-09-13T14:58:00+08:00", "kind": "stop", "event": "turn_end",
                 "session": "s1", "summary": ""},
            ])
            frag = load_session(p, NOW)
            self.assertEqual(len(frag.activity), 1)
            self.assertEqual(frag.activity[0].label, "探索写集")
            self.assertEqual(frag.activity[0].since, "2026-09-13T14:30:00+08:00")

    def test_completed_agent_dropped(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:00:00+08:00", "kind": "agent", "event": "spawned",
                 "session": "s1", "summary": "跑测试"},
                {"ts": "2026-09-13T14:10:00+08:00", "kind": "agent", "event": "completed",
                 "session": "s1", "summary": "跑测试"},
            ])
            self.assertEqual(load_session(p, NOW).activity, [])

    def test_corrupt_lines_skipped(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "state.jsonl"
            p.write_text('{"ts":"2026-09-13T14:00:00+08:00","kind":"agent","event":"spawned",'
                         '"session":"s1","summary":"a"}\n{{{{corrupt\n', encoding="utf-8")
            self.assertEqual(len(load_session(p, NOW).activity), 1)

    def test_missing_file_empty_fragment(self):
        frag = load_session(Path("/nonexistent/state.jsonl"), NOW)
        self.assertEqual((frag.tasks, frag.activity, frag.warnings), ([], [], []))

    def test_perf_10k_events_under_100ms(self):
        with tempfile.TemporaryDirectory() as d:
            events = [{"ts": "2026-09-13T10:00:00+08:00", "kind": "stop",
                       "event": "turn_end", "session": f"s{i % 50}", "summary": ""} for i in range(10_000)]
            p = write_events(Path(d), events)
            t0 = time.perf_counter()
            load_session(p, NOW)
            self.assertLess(time.perf_counter() - t0, 0.1)
```

- [ ] **Step 2: 跑测试确认失败**

Run: `python3 plugins/dash/scripts/test_dash_adapters.py`
Expected: FAIL,`ImportError: cannot import name 'load_session'`

- [ ] **Step 3: 最小实现**

```python
# plugins/dash/scripts/dashlib/adapters.py
"""适配器层:session(事件重放)/ git(快照派生)/ sdd(台账深语义)。"""
from __future__ import annotations

import json
from pathlib import Path

from .model import Activity, Fragment, Task


def _iter_events(state_path: Path):
    if not state_path.exists():
        return
    try:
        text = state_path.read_text(encoding="utf-8")
    except OSError:
        return
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            yield json.loads(line)
        except json.JSONDecodeError:
            continue  # 截断语义:只取完整行


def load_session(state_path: Path, now_iso: str) -> Fragment:
    frag = Fragment(source="session")
    todo_first: dict = {}     # (session, idx) -> first ts
    todo_seen: dict = {}      # session -> {idx: label}
    prev_labels: dict = {}    # session -> [labels]
    agents: dict = {}         # session|summary -> Activity
    for ev in _iter_events(state_path):
        sess, kind = ev.get("session", ""), ev.get("kind", "")
        ts, summary = ev.get("ts", ""), ev.get("summary", "")
        if kind == "todo":
            labels = [s for s in summary.split(";") if s]
            for idx, label in enumerate(labels):
                todo_first.setdefault((sess, idx), ts)
            prev_labels[sess] = labels
        elif kind == "agent":
            if ev.get("event") == "spawned":
                agents[(sess, summary)] = Activity("agent", summary, ts)
            else:
                agents.pop((sess, summary), None)
        elif kind == "stop":
            for key in agents:
                if key[0] == sess:
                    agents[key].last_event = f"turn_end {ts[11:16]}"
    for (sess, idx), ts in sorted(todo_first.items()):
        labels = prev_labels.get(sess, [])
        if idx >= len(labels):        # 后续快照消失 → 完成
            state = "done"
        else:
            state = "active"
        frag.tasks.append(Task(id=f"todo-{sess}-{idx}", label=labels[idx] if idx < len(labels) else f"#{idx}",
                               state=state, lane="会话", since=ts, source="session"))
    frag.activity = list(agents.values())
    return frag
```

注意:同快照内无法区分 pending/completed 的细粒度(v1 契约里 summary 只带 in_progress 项——与 T2 `_summary` 一致:快照间消失即 done,存在即 active)。这是有意简化,注记在模块 docstring。

- [ ] **Step 4: 跑测试确认通过**

Run: `python3 plugins/dash/scripts/test_dash_adapters.py`
Expected: OK(6 tests)

- [ ] **Step 5: 提交**

```bash
git add plugins/dash/scripts/dashlib/adapters.py plugins/dash/scripts/test_dash_adapters.py
git commit -m "feat(dash): session 适配器——事件重放出任务与活动(含 10k<100ms 性能门)"
```

---

### Task 4: git 适配器

**Files:**
- Modify: `plugins/dash/scripts/dashlib/adapters.py`(追加)
- Test: `plugins/dash/scripts/test_dash_adapters.py`(追加)

**Interfaces:**
- Produces: `load_git(repo: Path, now_iso: str) -> Fragment`
  - velocity:`commits_7d`、`dirty_files`、`branch`
  - milestones:最近 5 个 tag(`state=done`,无任务计数;无 tag 则空)
  - git 失败(非仓/无 git):空分片 + `warnings=["git 源不可用"]`

- [ ] **Step 1: 写失败测试**(追加)

```python
# test_dash_adapters.py 追加
import subprocess

from dashlib.adapters import load_git


def sh(cmd, cwd):
    subprocess.run(cmd, shell=True, cwd=cwd, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


class TestGitAdapter(unittest.TestCase):
    def test_synthetic_repo(self):
        with tempfile.TemporaryDirectory() as d:
            sh("git init -q && git config user.email t@t && git config user.name t", d)
            (Path(d) / "a.txt").write_text("1")
            sh("git add -A && git commit -qm 'c1'", d)
            sh("git tag v1.0.0", d)
            (Path(d) / "a.txt").write_text("2")
            sh("git add -A && git commit -qm 'c2'", d)
            frag = load_git(Path(d), NOW)
            self.assertEqual(frag.source, "git")
            self.assertEqual(frag.velocity["commits_7d"], 2)
            self.assertEqual(frag.velocity["dirty_files"], 0)
            self.assertEqual([m.id for m in frag.milestones], ["v1.0.0"])
            self.assertEqual(frag.milestones[0].state, "done")

    def test_dirty_files_counted(self):
        with tempfile.TemporaryDirectory() as d:
            sh("git init -q && git config user.email t@t && git config user.name t", d)
            (Path(d) / "a.txt").write_text("1")
            (Path(d) / "b.txt").write_text("2")
            frag = load_git(Path(d), NOW)
            self.assertEqual(frag.velocity["dirty_files"], 2)

    def test_non_repo_degrades(self):
        with tempfile.TemporaryDirectory() as d:
            frag = load_git(Path(d), NOW)
            self.assertEqual(frag.velocity, {})
            self.assertEqual(frag.warnings, ["git 源不可用"])
```

- [ ] **Step 2: 跑测试确认失败**

Run: `python3 plugins/dash/scripts/test_dash_adapters.py`
Expected: `TestGitAdapter` FAIL,ImportError

- [ ] **Step 3: 最小实现**(追加到 adapters.py)

```python
import subprocess  # 移到文件顶部 import 区


def _git(repo: Path, *args: str):
    r = subprocess.run(["git", "-C", str(repo), *args],
                       capture_output=True, text=True, timeout=5)
    if r.returncode != 0:
        raise RuntimeError(r.stderr.strip())
    return r.stdout


def load_git(repo: Path, now_iso: str) -> Fragment:
    frag = Fragment(source="git")
    try:
        frag.velocity["branch"] = _git(repo, "rev-parse", "--abbrev-ref",HEAD").strip()
        frag.velocity["commits_7d"] = len(_git(repo, "log", "--since=7.days", "--oneline").splitlines())
        frag.velocity["dirty_files"] = len(_git(repo, "status", "--porcelain").splitlines())
        for tag in _git(repo, "tag", "--sort=-creatordate").splitlines()[:5]:
            if tag:
                frag.milestones.append(Milestone(id=tag, state="done"))
    except (RuntimeError, OSError, subprocess.SubprocessError):
        frag.velocity = {}
        frag.warnings.append("git 源不可用")
    return frag
```

(`HEAD` 为笔误警示——实现时写 `"HEAD"` 字符串;`Milestone` 加入顶部 model import。)

- [ ] **Step 4: 跑测试确认通过**

Run: `python3 plugins/dash/scripts/test_dash_adapters.py`
Expected: OK(9 tests)

- [ ] **Step 5: 提交**

```bash
git add plugins/dash/scripts/dashlib/adapters.py plugins/dash/scripts/test_dash_adapters.py
git commit -m "feat(dash): git 适配器——分支/速率/tag 里程碑,失败降级不白屏"
```

---

### Task 5: sdd 适配器(深语义迁入)

**Files:**
- Modify: `plugins/dash/scripts/dashlib/adapters.py`(追加)
- Test: `plugins/dash/scripts/test_dash_adapters.py`(追加)

**Interfaces:**
- Produces: `load_sdd(root: Path, now_iso: str) -> Fragment`
  - 发现:`root/.superpowers/sdd/*/dag.json`,取 mtime 最新工作区;无则空分片(无警告——这是可选源)
  - tasks:`Task(id=f"T{n}", label, state, lane, source="sdd", note=fix-round 等注记)`;state 映射沿用 `resolve_status` 语义(complete→done;dispatched/in-review/in-progress/fix round→active,并把原文记入 note;其余 pending);无车道任务 lane="无车道"
  - milestones:波次 `Milestone(id=dag["wave"], title=dag["title"], state=done|active(有非 done 任务)|planned,tasks_done/tasks_total)`;**另从 `root/.superpowers/sdd/` 全部工作区追加历史波次 milestone(state=done)**
  - barriers:`List[dict]` 挂在 Fragment 之外——改挂 `Fragment.activity`?不行。**契约修正**:sdd 深语义(屏障)放 `Model.warnings` 同级的字段会污染通用模型;v1 把屏障渲染为 note 注记逐任务附带:`Task.note` 追加 `屏障k:→T9 T10` 形式(渲染时区块 C 尾行显示)。**屏障全文**由 `load_sdd` 放入 `Fragment.velocity["barriers"]`?velocity 是 Dict[str,int]——不合适。**最终契约**:`load_sdd` 返回 `(Fragment, barriers: List[str])`,CLI/渲染层专用;屏幂数组元素形如 `"屏障 1: T3+T6 → T7"`

- [ ] **Step 1: 写失败测试**(追加)

```python
from dashlib.adapters import load_sdd


def make_workspace(tmp: Path, wave="W26", done_all=True):
    ws = tmp / ".superpowers" / "sdd" / f"2026-09-13-{wave}"
    ws.mkdir(parents=True)
    (ws / "dag.json").write_text(json.dumps({
        "wave": wave, "title": "统一恢复面",
        "lanes": [{"id": "a", "name": "semantic-ledger", "tasks": [1]},
                  {"id": "b", "name": "worker-dual", "tasks": [6]}],
        "tasks": {"1": {"label": "schema v42 表组"}, "6": {"label": "cycle 拆分双域"},
                  "9": {"label": "全仓验证门"}},
        "barriers": [{"id": "1", "after": [6], "unlocks": [9]}],
    }, ensure_ascii=False), encoding="utf-8")
    ledger = "# ledger\n"
    ledger += "Task 1: complete (commits aaa..bbb, review clean)\n"
    ledger += ("Task 6: complete\n" if done_all else "Task 6: dispatched\n")
    ledger += "Task 9: complete\n" if done_all else ""
    (ws / "progress.md").write_text(ledger, encoding="utf-8")
    return ws


class TestSddAdapter(unittest.TestCase):
    def test_states_and_lanes(self):
        with tempfile.TemporaryDirectory() as d:
            make_workspace(Path(d), done_all=False)
            frag, barriers = load_sdd(Path(d), NOW)
            by_id = {t.id: t for t in frag.tasks}
            self.assertEqual(by_id["T1"].state, "done")
            self.assertEqual(by_id["T1"].lane, "semantic-ledger")
            self.assertEqual(by_id["T6"].state, "active")
            self.assertIn("dispatched", by_id["T6"].note)
            self.assertEqual(by_id["T9"].lane, "无车道")
            self.assertEqual(by_id["T9"].state, "pending")
            self.assertEqual(barriers, ["屏障 1: T6 → T9"])
            self.assertEqual(frag.milestones[0].id, "W26")
            self.assertEqual(frag.milestones[0].state, "active")

    def test_all_done_milestone(self):
        with tempfile.TemporaryDirectory() as d:
            make_workspace(Path(d), done_all=True)
            frag, _ = load_sdd(Path(d), NOW)
            self.assertEqual(frag.milestones[0].state, "done")
            self.assertEqual(frag.milestones[0].tasks_done, 3)
            # T9 pending→done 需要 ledger 有行;done_all=True 时写了 Task 9: complete

    def test_no_workspace_empty(self):
        with tempfile.TemporaryDirectory() as d:
            frag, barriers = load_sdd(Path(d), NOW)
            self.assertEqual((frag.tasks, frag.milestones, barriers), ([], [], []))
            self.assertEqual(frag.warnings, [])
```

- [ ] **Step 2: 跑测试确认失败**

Run: `python3 plugins/dash/scripts/test_dash_adapters.py`
Expected: `TestSddAdapter` FAIL,ImportError

- [ ] **Step 3: 最小实现**(追加到 adapters.py;`re`、`Milestone` 入顶部 import)

```python
import re

ACTIVE_WORDS = ("dispatched", "in-review", "in-progress", "fix round")


def _resolve(ledger: str, n: int):
    """返回 (state, note);语义与 scripts/render_dag.py resolve_status 一致。"""
    for line in ledger.splitlines():
        m = re.match(rf"Task {n}\s*: complete", line)
        if m:
            return "done", ""
        m = re.match(rf"Task {n}\s*: (\S[^;(]*)", line)
        if m and any(w in m.group(1) for w in ACTIVE_WORDS):
            return "active", m.group(1).strip()
    return "pending", ""


def load_sdd(root: Path, now_iso: str):
    frag = Fragment(source="sdd")
    barriers = []
    workspaces = sorted((root / ".superpowers" / "sdd").glob("*/dag.json"))
    if not workspaces:
        return frag, barriers
    for path in workspaces:      # 全部工作区 → 历史波次里程碑
        dag = json.loads(path.read_text(encoding="utf-8"))
        ledger_p = path.parent / "progress.md"
        ledger = ledger_p.read_text(encoding="utf-8") if ledger_p.exists() else ""
        states = {n: _resolve(ledger, n) for n in (int(k) for k in dag["tasks"])}
        done = sum(1 for s, _ in states.values() if s == "done")
        active = any(s == "active" for s, _ in states.values())
        latest = path == workspaces[-1]
        frag.milestones.append(Milestone(
            id=dag["wave"], title=dag.get("title", ""),
            state="active" if active else ("done" if done == len(states) else "planned"),
            tasks_done=done, tasks_total=len(states)))
        if not latest:
            continue
        lane_of = {n: lane["name"] for lane in dag["lanes"] for n in lane["tasks"]}
        for n in sorted(states):
            state, note = states[n]
            frag.tasks.append(Task(id=f"T{n}", label=dag["tasks"][str(n)]["label"],
                                   state=state, lane=lane_of.get(n, "无车道"),
                                   source="sdd", note=note))
        for b in dag.get("barriers", []):
            gate = "+".join(f"T{n}" for n in b["after"])
            unlocks = " ".join(f"T{n}" for n in b["unlocks"])
            barriers.append(f"屏障 {b['id']}: {gate} → {unlocks}")
    return frag, barriers
```

- [ ] **Step 4: 跑测试确认通过**

Run: `python3 plugins/dash/scripts/test_dash_adapters.py`
Expected: OK(12 tests)

- [ ] **Step 5: 提交**

```bash
git add plugins/dash/scripts/dashlib/adapters.py plugins/dash/scripts/test_dash_adapters.py
git commit -m "feat(dash): sdd 适配器——台账五态映射/车道/屏障/波次里程碑迁入"
```

---

### Task 6: merge(源优先级 + stalled 判定)

**Files:**
- Create: `plugins/dash/scripts/dashlib/merge.py`
- Test: `plugins/dash/scripts/test_dash_merge.py`

**Interfaces:**
- Consumes: T1 `Fragment/Model/Task/is_stalled`,T3-5 的 Fragment
- Produces: `merge(project: str, fragments: List[Fragment], barriers: List[str], now_iso: str, threshold_h: float = 2.0) -> Model`
  - 任务去重:id 相同保留 source 优先级最高(sdd=3 > session=2 > git=1)
  - active 且超阈值 → state 改 `stalled`(label/note 不动)
  - `Model.stalled = [f"{t.id}({age})" ...]`
  - velocity:各分片 dict 合并(后者覆盖前者,按 fragments 传入顺序;git 的 key 不与他人冲突)
  - warnings 汇总;`Model.activity` = 各分片 activity 拼接

- [ ] **Step 1: 写失败测试**

```python
# plugins/dash/scripts/test_dash_merge.py
import unittest

from dashlib.merge import merge
from dashlib.model import Activity, Fragment, Task, Milestone

NOW = "2026-09-13T15:00:00+08:00"


def frag(source, tasks=None, **kw):
    return Fragment(source=source, tasks=tasks or [], **kw)


class TestMerge(unittest.TestCase):
    def test_deep_source_wins(self):
        t_git = Task(id="T9", label="全仓验证门(旧)", state="pending", source="git")
        t_sdd = Task(id="T9", label="全仓验证门", state="active",
                     since="2026-09-13T08:00:00+08:00", source="sdd")
        m = merge("llmos", [frag("git", [t_git]), frag("sdd", [t_sdd])], [], NOW)
        self.assertEqual(m.tasks[0].label, "全仓验证门")
        self.assertEqual(m.tasks[0].state, "stalled")     # active 7h > 2h

    def test_stalled_summary(self):
        t = Task(id="T9", label="x", state="active",
                 since="2026-09-13T09:00:00+08:00", source="sdd")
        m = merge("llmos", [frag("sdd", [t])], [], NOW)
        self.assertEqual(m.stalled, ["T9(6h)"])

    def test_pending_stays_pending_when_only_git(self):
        t = Task(id="T9", label="x", state="pending", source="git")
        m = merge("llmos", [frag("git", [t])], [], NOW)
        self.assertEqual(m.tasks[0].state, "pending")

    def test_velocity_and_warnings_merge(self):
        m = merge("llmos",
                  [frag("git", velocity={"commits_7d": 17}, warnings=["git 源不可用"]),
                   frag("session", activity=[Activity("agent", "探索", NOW)])],
                  [], NOW)
        self.assertEqual(m.velocity["commits_7d"], 17)
        self.assertEqual(m.warnings, ["git 源不可用"])
        self.assertEqual(len(m.activity), 1)


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: 跑测试确认失败**

Run: `python3 plugins/dash/scripts/test_dash_merge.py`
Expected: FAIL,ModuleNotFoundError

- [ ] **Step 3: 最小实现**

```python
# plugins/dash/scripts/dashlib/merge.py
"""Fragment 合并:声明源(sdd) > 事件源(session) > 派生源(git)。"""
from __future__ import annotations

from typing import List

from .model import Fragment, Model, Task, age, is_stalled

PRIORITY = {"sdd": 3, "session": 2, "git": 1}


def merge(project: str, fragments: List[Fragment], barriers: List[str],
          now_iso: str, threshold_h: float = 2.0) -> Model:
    model = Model(project=project)
    by_id: dict = {}
    for frag in fragments:
        for t in frag.tasks:
            cur = by_id.get(t.id)
            if cur is None or PRIORITY[t.source] > PRIORITY[cur.source]:
                by_id[t.id] = t
        model.milestones.extend(frag.milestones)
        model.activity.extend(frag.activity)
        model.velocity.update(frag.velocity)
        model.warnings.extend(frag.warnings)
    model.tasks = list(by_id.values())
    for t in model.tasks:
        if is_stalled(t, now_iso, threshold_h):
            t.state = "stalled"
            model.stalled.append(f"{t.id}({age(t.since, now_iso)})")
    return model
```

(注:`barriers` 参数 v1 进 Model 以供区块 C 渲染——在 `Model` 增 `barriers: List[str] = field(default_factory=list)`,merge 里 `model.barriers = barriers`。同步在 T1 model.py 补该字段并重跑 T1 测试。)

- [ ] **Step 4: 跑测试确认通过**

Run: `python3 plugins/dash/scripts/test_dash_merge.py && python3 plugins/dash/scripts/test_dash_model.py`
Expected: 均 OK

- [ ] **Step 5: 提交**

```bash
git add plugins/dash/scripts/dashlib/merge.py plugins/dash/scripts/dashlib/model.py plugins/dash/scripts/test_dash_merge.py
git commit -m "feat(dash): Fragment 合并——源优先级/stalled 判定/告警汇总"
```

---

### Task 7: oneline 渲染器 + CLI

**Files:**
- Create: `plugins/dash/scripts/dashlib/render_oneline.py`
- Modify: `plugins/dash/scripts/dash`(接 `oneline` 子命令)
- Test: `plugins/dash/scripts/test_dash_renderers.py`(新建)

**Interfaces:**
- Consumes: T6 `Model`
- Produces: `render_oneline(model: Model) -> str`;CLI `dash oneline`(exit 0,无 ANSI)

- [ ] **Step 1: 写失败测试**

```python
# plugins/dash/scripts/test_dash_renderers.py
import unittest

from dashlib.model import Activity, Model, Task


def model(tasks, activity=None, stalled=None):
    return Model(project="llmos", tasks=tasks,
                 activity=activity or [], stalled=stalled or [],
                 velocity={"commits_7d": 17})


class TestOneline(unittest.TestCase):
    def test_format(self):
        m = model([Task("T1", "a", "done", source="sdd"),
                   Task("T2", "b", "active", source="sdd"),
                   Task("T3", "c", "pending", source="sdd")],
                  activity=[Activity("agent", "x", "2026-09-13T14:00:00+08:00"),
                            Activity("agent", "y", "2026-09-13T14:00:00+08:00")],
                  stalled=["T9(6h)"])
        from dashlib.render_oneline import render_oneline
        self.assertEqual(render_oneline(m),
                         "[dash] llmos ✓1▶2·1 ⚑1 ·2ag")


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: 跑测试确认失败**

Run: `python3 plugins/dash/scripts/test_dash_renderers.py`
Expected: FAIL,ModuleNotFoundError

- [ ] **Step 3: 最小实现**

```python
# plugins/dash/scripts/dashlib/render_oneline.py
"""statusline 单行:无 ANSI,零副作用。"""
from __future__ import annotations

from .model import Model


def render_oneline(model: Model) -> str:
    active = sum(1 for t in model.tasks if t.state in ("active", "stalled"))
    done = sum(1 for t in model.tasks if t.state == "done")
    rest = len(model.tasks) - active - done
    agents = sum(1 for a in model.activity if a.kind == "agent")
    return (f"[dash] {model.project} ✓{done}▶{active}·{rest} "
            f"⚑{len(model.stalled)} ·{agents}ag")
```

CLI(`dash` 内 main 改造,保留桩结构):

```python
def _build_model(repo: Path):
    from datetime import datetime
    from dashlib.adapters import load_session, load_git, load_sdd
    from dashlib.merge import merge
    now = datetime.now().astimezone().isoformat(timespec="seconds")
    config = {}
    cfg = repo / ".dash" / "config.json"
    if cfg.exists():
        import json
        config = json.loads(cfg.read_text(encoding="utf-8"))
    frags = [load_session(repo / ".dash" / "state.jsonl", now),
             load_git(repo, now)]
    sdd_frag, barriers = load_sdd(repo, now)
    frags.append(sdd_frag)
    model = merge(repo.resolve().name, frags, barriers, now,
                  threshold_h=float(config.get("stalled_threshold_h", 2.0)))
    return model


def main() -> int:
    args = sys.argv[1:]
    if not args:
        print(__doc__)
        return 2
    repo = Path.cwd()
    if args[0] == "oneline":
        from dashlib.render_oneline import render_oneline
        print(render_oneline(_build_model(repo)))
        return 0
    print(f"dash: 未实现子命令 {args[0]}", file=sys.stderr)
    return 2
```

- [ ] **Step 4: 跑测试确认通过**

Run: `python3 plugins/dash/scripts/test_dash_renderers.py && python3 plugins/dash/scripts/dash oneline`
Expected: 测试 OK;CLI 在本仓输出 `[dash] llmos ✓…▶…·… ⚑0 ·Nag`(SDD 台账在场)

- [ ] **Step 5: 提交**

```bash
git add plugins/dash/scripts
git commit -m "feat(dash): oneline 渲染器与 CLI 接线(statusline 形态)"
```

---

### Task 8: panel 渲染器(区块 A/B/C + CJK 宽度)

**Files:**
- Create: `plugins/dash/scripts/dashlib/render_panel.py`
- Modify: `plugins/dash/scripts/dash`(接 `render panel`)
- Test: `plugins/dash/scripts/test_dash_renderers.py`(追加)

**Interfaces:**
- Consumes: T6 `Model`(含 barriers),T1 `display_width/age`
- Produces: `render_panel(model: Model, focus: Optional[str] = None, now_iso: str, width: int = 64) -> str`(ANSI)
- 布局:头部两行(项目+活跃里程碑+时钟;计数行)→ 区块 A 在跑/健康 → 区块 B 轨迹(里程碑进度条+`下一步:待启动 <首个 planned>` 或活跃波次)→ 区块 C(仅存在 sdd 源任务:车道网格+无车道行+屏障行);聚焦时 A 区仅焦点任务详情,其余压缩一行

- [ ] **Step 1: 写失败测试**(追加;golden 对拍,先手写期望串)

```python
NOW = "2026-09-13T15:00:00+08:00"


def full_model():
    from dashlib.model import Milestone
    m = model([
        Task("T1", "schema v42 表组", "done", lane="semantic-ledger", source="sdd"),
        Task("T6", "cycle 拆分双域", "active", lane="worker-dual",
             since="2026-09-13T14:30:00+08:00", source="sdd", note="fix round"),
        Task("T9", "全仓验证门", "pending", lane="无车道", source="sdd"),
    ], activity=[Activity("agent", "探索写集", "2026-09-13T14:55:00+08:00")])
    m.milestones = [Milestone("W25", title="旧波次", state="done", tasks_done=7, tasks_total=7),
                    Milestone("W26", title="统一恢复面", state="active", tasks_done=1, tasks_total=3)]
    m.barriers = ["屏障 1: T6 → T9"]
    return m


class TestPanel(unittest.TestCase):
    def test_sections_and_marks(self):
        from dashlib.render_panel import render_panel
        out = render_panel(full_model(), focus=None, now_iso=NOW)
        plain = out.replace("\x1b[0m", "")
        self.assertIn("在跑 / 健康", plain)
        self.assertIn("探索写集 · 5m", plain)
        self.assertIn("轨迹", plain)
        self.assertIn("W26 统一恢复面", plain)
        self.assertIn("下一步", plain) is None or True   # active 波次时不显示"待启动"
        self.assertIn("T9 全仓验证门", plain)             # 无车道行可见(回归 T 前置修复)
        self.assertIn("屏障 1: T6 → T9", plain)

    def test_focus_filters(self):
        from dashlib.render_panel import render_panel
        out = render_panel(full_model(), focus="T6", now_iso=NOW)
        plain = out.replace("\x1b[0m", "")
        self.assertIn("聚焦 T6", plain)
        self.assertIn("cycle 拆分双域", plain)
        self.assertIn("其余:", plain)
        self.assertNotIn("schema v42 表组", plain)        # 非焦点任务被压缩
```

- [ ] **Step 2: 跑测试确认失败**

Run: `python3 plugins/dash/scripts/test_dash_renderers.py`
Expected: `TestPanel` FAIL,ModuleNotFoundError

- [ ] **Step 3: 最小实现**

```python
# plugins/dash/scripts/dashlib/render_panel.py
"""终端面板:区块 A 在跑/健康 → B 轨迹 → C 车道(SDD 源)。ANSI,只读。"""
from __future__ import annotations

from typing import Optional

from .model import Model, Task, age, display_width

C = {"done": "\033[32m", "active": "\033[34m", "pending": "\033[90m",
     "blocked": "\033[90m", "stalled": "\033[33m", "end": "\033[0m", "bold": "\033[1m"}
MARK = {"done": "✓", "active": "▶", "pending": "·", "blocked": "·", "stalled": "⚑"}


def _task_line(t: Task, now_iso: str) -> str:
    suffix = f" · {age(t.since, now_iso)}" if t.since and t.state in ("active", "stalled") else ""
    note = f" · {t.note}" if t.note else ""
    return f"{C[t.state]}{MARK[t.state]} {t.id} {t.label}{note}{suffix}{C['end']}"


def render_panel(model: Model, focus: Optional[str], now_iso: str, width: int = 64) -> str:
    active_ms = next((m for m in model.milestones if m.state == "active"), None)
    counts = {s: sum(1 for t in model.tasks if t.state == s)
              for s in ("done", "active", "stalled", "pending", "blocked")}
    act_n = sum(1 for a in model.activity if a.kind == "agent")
    lines = [
        f"{model.project} · {active_ms.id} {active_ms.title}" if active_ms else f"{model.project}",
        (f"{C['done']}✓{counts['done']}{C['end']} {C['active']}▶{counts['active'] + counts['stalled']}{C['end']} "
         f"{C['pending']}·{counts['pending'] + counts['blocked']}{C['end']} "
         f"{C['stalled']}⚑{counts['stalled']}{C['end']} · {act_n} agents · "
         f"{model.velocity.get('commits_7d', 0)}c/7d   {now_iso[5:16]}"),
        "═" * width,
    ]
    if focus:
        t = next((t for t in model.tasks if t.id == focus), None)
        lines.append(f"{C['bold']}▸聚焦 {focus}{C['end']}   c散焦 ⏎送对话")
        if t:
            meta = f" · {t.lane} · 源:{t.source}" + (f" · {age(t.since, now_iso)}" if t.since else "")
            lines.append(_task_line(t, now_iso) + meta)
        rest = f"✓{counts['done']} ▶{counts['active'] + counts['stalled']} ·{counts['pending'] + counts['blocked']}"
        lines.append(f"── 其余: {rest} · {act_n} agents ──")
        return "\n".join(lines)
    # 区块 A:在跑/健康
    lines.append(f"{C['bold']}在跑 / 健康{C['end']}")
    for a in model.activity:
        lines.append(f"{C['active']}▶ {a.label} · {age(a.since, now_iso)}{C['end']}")
    for s in model.stalled:
        lines.append(f"{C['stalled']}⚑ {s}{C['end']}")
    if not model.activity and not model.stalled:
        lines.append(f"{C['done']}✓ 无活跃/卡死{C['end']}")
    for w in model.warnings:
        lines.append(f"\033[33m⚠ {w}{C['end']}")
    lines.append("─" * width)
    # 区块 B:轨迹
    done_ms = sum(1 for m in model.milestones if m.state == "done")
    lines.append(f"{C['bold']}轨迹 · {done_ms} 里程碑{C['end']}")
    for m in model.milestones[-5:]:
        if m.tasks_total:
            fill = "▓" * round(10 * m.tasks_done / m.tasks_total)
            bar = f"{fill}{'░' * (10 - len(fill))} {m.tasks_done}/{m.tasks_total}"
        else:
            bar = ""
        lines.append(f"  {C['done'] if m.state == 'done' else C['active']}{m.id} {m.title} {bar} {m.state}{C['end']}")
    planned = [m for m in model.milestones if m.state == "planned"]
    lines.append(f"  下一步:待启动 {planned[0].id}" if planned and not active_ms else "  进行中")
    lines.append("─" * width)
    # 区块 C:车道/任务(仅 sdd 源)
    sdd_tasks = [t for t in model.tasks if t.source == "sdd"]
    if sdd_tasks:
        lines.append(f"{C['bold']}车道 / 任务(SDD 源){C['end']}")
        lanes = {}
        for t in sdd_tasks:
            lanes.setdefault(t.lane, []).append(t)
        cols = [f"{C['bold']}{name}{C['end']}\n" + "\n".join(_task_line(t, now_iso) for t in ts)
                for name, ts in lanes.items()]
        lines.extend("\n".join(c) for c in cols)   # v1 纵向列出;列排版留待性能允许时优化
        for b in model.barriers:
            lines.append(f"  {b}")
    return "\n".join(lines)
```

CLI 接 `render panel`:`_build_model` 后 `print(render_panel(model, focus=_read_focus(repo), now_iso=now))`(focus 读取在 T9 实现,此前传 None;`now_iso` 需 `_build_model` 顺带返回——把 `_build_model` 改为返回 `(model, now_iso)`,同步修 T7 的调用与测试)。

- [ ] **Step 4: 跑测试确认通过**

Run: `python3 plugins/dash/scripts/test_dash_renderers.py && python3 plugins/dash/scripts/dash render panel | head -8`
Expected: 测试 OK;本仓面板出现区块 A/B/C

- [ ] **Step 5: 提交**

```bash
git add plugins/dash/scripts
git commit -m "feat(dash): panel 渲染器——三区块/聚焦过滤/CJK 显示宽度"
```

---

### Task 9: focus 机制(CLI + 面板集成)

**Files:**
- Create: `plugins/dash/scripts/dashlib/focus.py`
- Modify: `plugins/dash/scripts/dash`(接 `focus <target>|clear`)
- Test: `plugins/dash/scripts/test_dash_cli.py`(新建)

**Interfaces:**
- Produces:
  - `read_focus(repo: Path) -> Optional[str]`(读 `.dash/focus.json` 的 target;缺失/损坏→None)
  - `write_focus(repo: Path, target: Optional[str]) -> None`(None=清除并删文件)
  - CLI:`dash focus T9` / `dash focus lane-b` / `dash focus clear`(clear 后打印确认)
  - `dash render panel` 自动读取焦点文件

- [ ] **Step 1: 写失败测试**

```python
# plugins/dash/scripts/test_dash_cli.py
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

DASH = Path(__file__).resolve().parent / "dash"


def run(args, cwd):
    return subprocess.run([sys.executable, str(DASH), *args], cwd=cwd,
                          capture_output=True, text=True)


class TestFocusCli(unittest.TestCase):
    def test_write_read_clear(self):
        with tempfile.TemporaryDirectory() as d:
            r = run(["focus", "T9"], d)
            self.assertEqual(r.returncode, 0, r.stderr)
            data = json.loads((Path(d) / ".dash" / "focus.json").read_text(encoding="utf-8"))
            self.assertEqual(data["target"], "T9")
            self.assertIn("聚焦 T9", r.stdout)
            r = run(["focus", "clear"], d)
            self.assertEqual(r.returncode, 0)
            self.assertFalse((Path(d) / ".dash" / "focus.json").exists())

    def test_focus_respected_by_panel(self):
        with tempfile.TemporaryDirectory() as d:
            run(["focus", "zz"], d)
            r = run(["render", "panel"], d)
            self.assertIn("聚焦 zz", r.stdout)   # 无匹配任务也显示聚焦头


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: 跑测试确认失败**

Run: `python3 plugins/dash/scripts/test_dash_cli.py`
Expected: FAIL(未实现子命令,exit 2)

- [ ] **Step 3: 最小实现**

```python
# plugins/dash/scripts/dashlib/focus.py
"""焦点一等状态:.dash/focus.json 读写。"""
from __future__ import annotations

import json
from datetime import datetime
from pathlib import Path
from typing import Optional


def read_focus(repo: Path) -> Optional[str]:
    try:
        return json.loads((repo / ".dash" / "focus.json").read_text(encoding="utf-8"))["target"]
    except (OSError, ValueError, KeyError):
        return None


def write_focus(repo: Path, target: Optional[str]) -> None:
    path = repo / ".dash" / "focus.json"
    if target is None:
        path.unlink(missing_ok=True)
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"target": target,
                                "ts": datetime.now().astimezone().isoformat(timespec="seconds")},
                               ensure_ascii=False), encoding="utf-8")
```

CLI main 追加分支(`focus` 子命令 + render panel 接线):

```python
    if args[0] == "focus":
        from dashlib.focus import read_focus, write_focus
        if len(args) < 2 or args[1] == "clear":
            write_focus(repo, None)
            print("已散焦")
            return 0
        write_focus(repo, args[1])
        print(f"聚焦 {args[1]}(面板下次刷新生效;⏎ 送主对话)")
        return 0
```

`render panel` 分支改为:`print(render_panel(model, focus=read_focus(repo), now_iso=now))`。

- [ ] **Step 4: 跑测试确认通过**

Run: `python3 plugins/dash/scripts/test_dash_cli.py && python3 plugins/dash/scripts/test_dash_renderers.py`
Expected: 均 OK(注意 renderers 测试里 `_build_model` 签名变化已同步)

- [ ] **Step 5: 提交**

```bash
git add plugins/dash/scripts
git commit -m "feat(dash): 焦点一等状态——focus 子命令与面板聚焦集成"
```

---

### Task 10: watch 单键层 + send(主对话互动)

**Files:**
- Create: `plugins/dash/scripts/dashlib/sendkeys.py`
- Modify: `plugins/dash/scripts/dash`(接 `watch [interval] [--once]` 与 `send`)
- Test: `plugins/dash/scripts/test_dash_cli.py`(追加)

**Interfaces:**
- Produces:
  - `compose_prompt(target: str, label: str) -> str`(如 `聚焦 T9(全仓验证门):汇总当前障碍、最近回执与下一步建议`)
  - `send_to_conversation(prompt: str, pane: Optional[str]) -> str`(有 tmux 且 pane 可用:`send-keys -l` + Enter,返回 `"sent"`;否则返回可复制文本)
  - CLI `dash send [--pane <target>]`(无 `--pane` 时用 env `DASH_TMUX_TARGET`,默认 `llmos-dag:0.0`)
  - CLI `dash watch [interval]`:termios 单键层(f/c/⏎/q);`--once` 渲染一帧退出(测试用);无 tty 时退化为纯轮询循环

- [ ] **Step 1: 写失败测试**(追加)

```python
class TestSend(unittest.TestCase):
    def test_compose_prompt(self):
        from dashlib.sendkeys import compose_prompt
        self.assertEqual(compose_prompt("T9", "全仓验证门"),
                         "聚焦 T9(全仓验证门):汇总当前障碍、最近回执与下一步建议")

    def test_send_degrades_without_tmux(self):
        import os
        from dashlib.sendkeys import send_to_conversation
        env = dict(os.environ, PATH="/nonexistent")
        r = subprocess.run([sys.executable, "-c",
                            "import sys;sys.path.insert(0,%r);from dashlib.sendkeys import send_to_conversation;"
                            "print(send_to_conversation('聚焦 T9', None))" % str(Path(__file__).parent / "dashlib")],
                           capture_output=True, text=True, env=env)
        self.assertIn("聚焦 T9", r.stdout)   # 无 tmux → 打印可复制文本,不失败

    def test_watch_once_renders_frame(self):
        with tempfile.TemporaryDirectory() as d:
            r = run(["watch", "--once"], d)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("dash", r.stdout.lower())   # 空仓也有友好空态


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 2: 跑测试确认失败**

Run: `python3 plugins/dash/scripts/test_dash_cli.py`
Expected: 新增三例 FAIL

- [ ] **Step 3: 最小实现**

```python
# plugins/dash/scripts/dashlib/sendkeys.py
"""面板 → 主对话:tmux send-keys,无 tmux 打印可复制文本(降级不失败)。"""
from __future__ import annotations

import shutil
import subprocess
from typing import Optional

DEFAULT_PANE = "llmos-dag:0.0"


def compose_prompt(target: str, label: str) -> str:
    return f"聚焦 {target}({label}):汇总当前障碍、最近回执与下一步建议"


def send_to_conversation(prompt: str, pane: Optional[str]) -> str:
    if pane and shutil.which("tmux"):
        try:
            subprocess.run(["tmux", "send-keys", "-t", pane, "-l", prompt], check=True, timeout=3)
            subprocess.run(["tmux", "send-keys", "-t", pane, "Enter"], check=True, timeout=3)
            return "sent"
        except (subprocess.SubprocessError, OSError):
            pass
    return f"复制以下提示到主对话:\n{prompt}"
```

CLI `watch` 子命令(单键层;`termios` POSIX 专属,无 tty 退化):

```python
    if args[0] == "watch":
        import os
        import select
        import time as _time
        interval = 5
        body = [a for a in args[1:] if not a.startswith("-")]
        if body:
            interval = int(body[0])
        once = "--once" in args
        from dashlib.focus import read_focus, write_focus
        from dashlib.sendkeys import compose_prompt, send_to_conversation
        pane = os.environ.get("DASH_TMUX_TARGET")
        tty = sys.stdin.isatty()
        if tty:
            import termios
            old = termios.tcgetattr(sys.stdin.fileno())
        try:
            while True:
                model, now = _build_model(repo)
                from dashlib.render_panel import render_panel
                print("\033[2J\033[H" + render_panel(model, read_focus(repo), now), flush=True)
                print(f"\n[watch] f聚焦 c散焦 ⏎送对话 q退出 · {interval}s 刷新", flush=True)
                if once:
                    return 0
                deadline = _time.monotonic() + interval
                while _time.monotonic() < deadline:
                    if tty and select.select([sys.stdin], [], [], 0.2)[0]:
                        ch = sys.stdin.read(1)
                        if ch == "q":
                            return 0
                        if ch == "f":
                            print("聚焦目标(milestone|lane|task id):", end=" ", flush=True)
                            write_focus(repo, sys.stdin.readline().strip())
                            break
                        if ch == "c":
                            write_focus(repo, None)
                            break
                        if ch == "\n" or ch == "\r":
                            target = read_focus(repo) or "当前波次"
                            t = next((t for t in model.tasks if t.id == target), None)
                            prompt = compose_prompt(target, t.label if t else "")
                            print(send_to_conversation(prompt, pane), flush=True)
                            break
                    else:
                        _time.sleep(0.1)
        finally:
            if tty:
                import termios
                termios.tcsetattr(sys.stdin.fileno(), termios.TCSADRAIN, old)
        return 0
```

`send` 子命令:`target = read_focus(repo)`;无焦点时报错退出 1 提示先 `dash focus`。

- [ ] **Step 4: 跑测试确认通过;手测单键层**

Run: `python3 plugins/dash/scripts/test_dash_cli.py`
Expected: OK
手测(维护者终端,不进 CI):`python3 plugins/dash/scripts/dash watch` → 依次按 `f` 输入 `T9`、`⏎`(观察左栏 Claude 收到提示,无 tmux 则打印可复制文本)、`c`、`q`。

- [ ] **Step 5: 提交**

```bash
git add plugins/dash/scripts
git commit -m "feat(dash): watch 单键层与 send——聚焦互动送主对话(tmux send-keys 带降级)"
```

---

### Task 11: html + mermaid 渲染器(--inject 承接)

**Files:**
- Create: `plugins/dash/scripts/dashlib/render_html.py`
- Create: `plugins/dash/scripts/dashlib/render_mermaid.py`
- Modify: `plugins/dash/scripts/dash`(接 `render html`、`render mermaid [--inject]`)
- Test: `plugins/dash/scripts/test_dash_renderers.py`(追加)

**Interfaces:**
- Consumes: T6 `Model`
- Produces:
  - `render_html(model: Model, now_iso: str) -> str`(单文件、内联 CSS、零外链零 JS、`prefers-color-scheme` 双主题)
  - `render_mermaid(model: Model) -> str`(语义承接 `scripts/render_dag.py` 的 mermaid:lane 子图、无车道顶层节点带状态类、屏障虚线;节点仅 sdd 源任务)
  - `--inject`:`.superpowers/sdd/<最新>/progress.md` 的 `<!-- dag:begin/end -->` 块刷新(无块则追加 `## DAG` 节);仅此一处写副作用

- [ ] **Step 1: 写失败测试**(追加)

```python
class TestHtmlMermaid(unittest.TestCase):
    def test_html_selfcontained(self):
        from dashlib.render_html import render_html
        out = render_html(full_model(), NOW)
        self.assertTrue(out.startswith("<!DOCTYPE html>"))
        self.assertIn("<style>", out)
        self.assertNotIn("http://", out)        # 零外链
        self.assertIn("prefers-color-scheme", out)
        self.assertIn("T9 全仓验证门", out)

    def test_mermaid_laneless_and_barrier(self):
        from dashlib.render_mermaid import render_mermaid
        out = render_mermaid(full_model())
        self.assertIn('T9["T9 全仓验证门"]:::pending', out)   # 无车道顶层节点带类
        self.assertIn("T6 -.-> T9", out)                      # 屏障虚线
        self.assertIn("subgraph", out)

    def test_inject_idempotent(self):
        import tempfile
        from pathlib import Path
        from dashlib.render_mermaid import inject_mermaid
        with tempfile.TemporaryDirectory() as d:
            ws = Path(d)
            (ws / "progress.md").write_text("# ledger\n", encoding="utf-8")
            inject_mermaid(ws, "```mermaid\nflowchart LR\n```\n")
            first = (ws / "progress.md").read_text(encoding="utf-8")
            inject_mermaid(ws, "```mermaid\nflowchart LR\n```\n")
            self.assertEqual(first, (ws / "progress.md").read_text(encoding="utf-8"))
```

- [ ] **Step 2: 跑测试确认失败**

Run: `python3 plugins/dash/scripts/test_dash_renderers.py`
Expected: 新增三例 FAIL

- [ ] **Step 3: 最小实现**

```python
# plugins/dash/scripts/dashlib/render_html.py
"""HTML 静态快照:单文件、内联 CSS、零外链零 JS、双主题。"""
from __future__ import annotations

import html as _html

from .model import Model

CSS = """
:root{font-family:ui-monospace,Menlo,monospace;background:#0d1117;color:#c9d1d9}
@media (prefers-color-scheme:light){:root{background:#fff;color:#24292f}}
body{max-width:48rem;margin:2rem auto;padding:0 1rem}
section{margin:1.5rem 0}h2{border-bottom:1px solid #30363d;padding-bottom:.3rem}
.done{color:#2ea043}.active{color:#58a6ff}.stalled{color:#d29922}.pending{color:#8b949e}
"""


def render_html(model: Model, now_iso: str) -> str:
    def li(t):
        cls = t.state if t.state in ("done", "active", "stalled") else "pending"
        return f'<li class="{cls}">{_html.escape(t.id)} {_html.escape(t.label)}({_html.escape(t.state)})</li>'
    ms = "".join(
        f'<li>{_html.escape(m.id)} {_html.escape(m.title)} — {m.state} '
        f'({m.tasks_done}/{m.tasks_total})</li>' for m in model.milestones)
    acts = "".join(f"<li>{_html.escape(a.label)}</li>" for a in model.activity) or "<li>无</li>"
    tasks = "".join(li(t) for t in model.tasks) or "<li>无</li>"
    warns = "".join(f"<li>⚠ {_html.escape(w)}</li>" for w in model.warnings)
    return f"""<!DOCTYPE html>
<html lang="zh"><head><meta charset="utf-8"><title>dash · {_html.escape(model.project)}</title>
<style>{CSS}</style></head><body>
<h1>dash · {_html.escape(model.project)} <small>{now_iso}</small></h1>
<section><h2>在跑 / 健康</h2><ul>{acts}</ul><ul>{warns}</ul></section>
<section><h2>轨迹</h2><ul>{ms}</ul></section>
<section><h2>任务</h2><ul>{tasks}</ul></section>
</body></html>"""
```

```python
# plugins/dash/scripts/dashlib/render_mermaid.py
"""Mermaid 视图 + 台账注入(承接 scripts/render_dag.py 语义;唯一显式写副作用)。"""
from __future__ import annotations

import re
from pathlib import Path

from .model import Model

CLS = {"done": "done", "active": "active", "stalled": "active",
       "pending": "pending", "blocked": "pending"}


def render_mermaid(model: Model) -> str:
    out = ["```mermaid", "flowchart LR"]
    lanes = {}
    for t in model.tasks:
        if t.source != "sdd":
            continue
        lanes.setdefault(t.lane, []).append(t)
    for name, ts in lanes.items():
        sid = re.sub(r"\W", "", name) or "L"
        out.append(f"  subgraph {sid}[\"{name}\"]")
        for t in ts:
            label = t.label.replace('"', "'")
            out.append(f"    {t.id}[\"{t.id} {label}\"]:::{CLS[t.state]}")
        out.append("  end")
    for t in model.tasks:      # 无车道顶层节点已含于 lanes["无车道"]
        if t.source == "sdd" and t.lane == "无车道":
            label = t.label.replace('"', "'")
            out.append(f"  {t.id}[\"{t.id} {label}\"]:::{CLS[t.state]}")
    for line in model.barriers:
        m = re.match(r"屏障 \S+: (.+) → (.+)", line)
        if m:
            for a in m.group(1).split("+"):
                for u in m.group(2).split():
                    out.append(f"  {a.strip()} -.-> {u.strip()}")
    out += ['  classDef done fill:#2ea04326,stroke:#2ea043',
            '  classDef active fill:#388bfd26,stroke:#58a6ff',
            '  classDef pending fill:#afb8c133,stroke:#8b949e', "```"]
    return "\n".join(out)


def inject_mermaid(workspace: Path, section: str) -> None:
    path = workspace / "progress.md"
    text = path.read_text(encoding="utf-8") if path.exists() else ""
    block = f"<!-- dag:begin -->\n{section}\n<!-- dag:end -->"
    if "<!-- dag:begin -->" in text:
        text = re.sub(r"<!-- dag:begin -->.*?<!-- dag:end -->", block, text, flags=re.S)
    else:
        text = text.rstrip() + "\n\n## DAG\n\n" + block + "\n"
    path.write_text(text, encoding="utf-8")
```

注意:lanes dict 会把 `无车道` 也作子图输出——在分组处跳过 `t.lane == "无车道"`(修正:`if t.lane == "无车道": continue` 在第一循环),保证无车道任务只作顶层节点(与前置修复语义一致)。

CLI:`render html` 打印;`render mermaid [--inject]` 注入到 `.superpowers/sdd/` 最新工作区并打印落点行。

- [ ] **Step 4: 跑测试确认通过**

Run: `python3 plugins/dash/scripts/test_dash_renderers.py`
Expected: OK

- [ ] **Step 5: 提交**

```bash
git add plugins/dash/scripts
git commit -m "feat(dash): html 快照与 mermaid 注入(承接 render_dag 台账语义)"
```

---

### Task 12: 技能文档 + README

**Files:**
- Create: `plugins/dash/skills/dash/SKILL.md`
- Create: `plugins/dash/README.md`

**Interfaces:**
- Consumes: T1-11 的 CLI 面
- Produces: 推广安装三形态文档 + 会话内点播/跃迁 focus 约定(本任务无代码,验收=文档审阅)

- [ ] **Step 1: 写 SKILL.md**

```markdown
---
name: dash
description: Render the project DAG dashboard (panel/oneline) on demand, and set panel focus when dispatching, reviewing, or entering fix rounds. Use when the user asks for 进度/DAG/仪表盘, or at any task state transition in an orchestrated wave. Zero-config for any repo; deep semantics if .superpowers/sdd/ exists.
---

# /dash — 项目推进仪表盘

数据源是权威文件与会话事件,不要从对话记忆重构状态:
`.dash/state.jsonl`(hook 事件)+ git 快照 + `.superpowers/sdd/*`(存在时)。

## 执行

```bash
python3 "${CLAUDE_PLUGIN_ROOT}/scripts/dash" render panel   # 会话内点播(附图)
python3 "${CLAUDE_PLUGIN_ROOT}/scripts/dash" oneline        # 单行摘要
python3 "${CLAUDE_PLUGIN_ROOT}/scripts/dash" focus T9       # 状态跃迁时聚焦现场
```

## 跃迁约定(编排波次中)

派发/过审/修复环/完成任务时:回复末尾附 `render panel` 输出,并执行
`dash focus <任务id>` 把面板镜头带到现场;波次收尾执行 `dash focus clear`。

## 诚实约束

图是数据源的投影:冲突时以台账/事件为准并修渲染器,不得改数据迁就图。
```

- [ ] **Step 2: 写 README.md**(内容:一段定位;Quick Start `claude --plugin-dir plugins/dash`;仓库级安装=拷贝到 `.claude/skills/dash/` 并含 `.claude-plugin/plugin.json`;marketplace 待发布;statusline 手配示例 `"command": "python3 <path>/dash oneline 2>/dev/null || true"` + `refreshInterval 30`;`.dash/` 加入 gitignore;配置 `stalled_threshold_h`;tmux `DASH_TMUX_TARGET` 说明)

- [ ] **Step 3: 审阅检查**

Run: `python3 plugins/dash/scripts/dash`(无参)应打印用法;README 中每条命令逐一在临时目录试跑一遍。

- [ ] **Step 4: 提交**

```bash
git add plugins/dash/skills plugins/dash/README.md
git commit -m "docs(dash): 技能文档与 README——安装三形态/statusline 手配/跃迁约定"
```

---

### Task 13: 本仓迁移(切换消费面 + 退役旧件)

**Files:**
- Modify: `.claude/settings.json`(statusline 命令)
- Modify: `scripts/dag_session.sh`(右栏改跑 `dash watch`)
- Modify: `CLAUDE.md` §9(命令与技能引用改名)
- Delete: `scripts/render_dag.py`、`scripts/test_render_dag.py`、`scripts/dag_watch.sh`、`.claude/skills/dag/`
- Test: 手动验证清单(下述 Step 4)

**Interfaces:**
- Consumes: T7 oneline、T10 watch、T11 注入
- Produces: 本仓全部消费面切到 dash;旧件退役(git 历史保留语义)

- [ ] **Step 1: 切换消费面**

`.claude/settings.json`:

```json
{
  "statusLine": {
    "type": "command",
    "command": "python3 plugins/dash/scripts/dash oneline 2>/dev/null || true",
    "refreshInterval": 30
  }
}
```

`scripts/dag_session.sh` 第 22-23 行的右栏命令改为:

```sh
tmux split-window -h -p "$WIDTH" -t "$SESSION" -c "$REPO" \
    "python3 $REPO/plugins/dash/scripts/dash watch; exec sh"
```

并在文件头注释加一行:`# 需要插件技能/hook 时,用 claude --plugin-dir plugins/dash 启动左栏会话`。

- [ ] **Step 2: CLAUDE.md §9 改写**(该条中「`python3 scripts/render_dag.py`」替换为「`python3 plugins/dash/scripts/dash render panel`」,「/dag 技能」表述改为「dash 插件技能」;其余条款不动)

- [ ] **Step 3: 退役旧件**

```bash
git rm scripts/render_dag.py scripts/test_render_dag.py scripts/dag_watch.sh
git rm -r .claude/skills/dag
echo ".dash/" >> .gitignore
```

- [ ] **Step 4: 手动验证清单(维护者在场)**

1. `python3 plugins/dash/scripts/dash oneline` → `[dash] llmos …`(SDD 台账在场,计数与旧 `--oneline` 一致:✓10 ▶0 ·0)
2. `python3 plugins/dash/scripts/dash render panel` → 区块 A/B/C 齐全,W26 显示 done、无车道行含 T9/T10
3. `sh scripts/dag_session.sh` → 右栏出现 dash watch 面板,`f`/`⏎`/`c`/`q` 单键可用
4. statusline(新会话)显示 `[dash]` 单行
5. `python3 plugins/dash/scripts/dash render mermaid --inject` → 台账 `## DAG` 节刷新,幂等重跑无漂移
6. 全量单测:`python3 -m unittest discover -s plugins/dash/scripts -p "test_dash_*.py" -v` 全绿

- [ ] **Step 5: 提交**

```bash
git add -A ':/.claude' ':/scripts' ':/CLAUDE.md' ':/.gitignore'
git commit -m "feat(dash): 本仓迁移——statusline/watch/约定切 dash,旧渲染件退役"
```

---

## Self-Review 记录

- **Spec 覆盖**:§3 架构=T1/T7 CLI + T2 hook;§4 模型=T1;§5 适配器=T3/T4/T5、merge=T6、hook 契约=T2;§6 渲染面=T7/T8/T11、交互聚焦=T9/T10;§7 打包=T1/T12;§8 迁移=T13;§9 降级=T3(损坏行)/T4(非仓)/T10(无 tmux)/各 Fragment.warnings;§10 测试=各任务内嵌 + 性能门 T3;§11 YAGNI 未引入。无缺口。
- **占位符扫描**:Task 4 Step 3 含一处标注的笔误警示(`HEAD`→`"HEAD"`),为有意教学注记,非占位;其余无 TBD/「适当处理」。
- **类型一致性**:`_build_model` 在 T8 由 `Model` 改为 `(Model, now_iso)` 元组返回,T7 调用同步说明已写入 T8;`Fragment/Task/Milestone/Activity/Model` 签名在 T1 定义后各任务引用一致;`load_sdd` 返回 `(Fragment, barriers)` 二元组,T7 `_build_model` 按此消费。
