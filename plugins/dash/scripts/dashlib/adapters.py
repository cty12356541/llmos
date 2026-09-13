# plugins/dash/scripts/dashlib/adapters.py
"""适配器层:session(事件重放)/ git(快照派生)/ sdd(台账深语义)。

v1 简化注记:todo 快照 summary 只带 in_progress 项(R10 起,空串=无在途),
以标签身份跨快照配对——最新快照存在即 active,缺席即 done。
"""
from __future__ import annotations

import json
import re
import subprocess
from datetime import datetime
from pathlib import Path

from .model import Activity, Fragment, Milestone, Task


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
    first_seen: dict = {}     # (session, label) -> first ts
    first_order: dict = {}    # session -> [labels 首现顺序]
    latest: dict = {}         # session -> 最新快照 labels(在途集合)
    agents: dict = {}         # session|summary -> Activity(插入序=生成序)
    for ev in _iter_events(state_path):
        sess, kind = ev.get("session", ""), ev.get("kind", "")
        ts, summary = ev.get("ts", ""), ev.get("summary", "")
        if kind == "todo":
            labels = [s for s in summary.split(";") if s]
            for label in labels:
                first_seen.setdefault((sess, label), ts)
                if label not in first_order.setdefault(sess, []):
                    first_order[sess].append(label)
            latest[sess] = labels        # 最新快照=在途集合(R10)
        elif kind == "agent":
            if ev.get("event") == "spawned":
                agents[(sess, summary)] = Activity("agent", summary, ts)
            elif summary:                # 带摘要:按 (session, summary) 精确配对
                agents.pop((sess, summary), None)
            else:                        # 空摘要(SubagentStop):FIFO 弹出最早仍在途的(R9)
                for key in agents:
                    if key[0] == sess:
                        agents.pop(key)
                        break
        elif kind == "stop":
            # R19:turn_end 先标注残余 agent 的最后事件,再整会话清落
            # (前台 agent 不活过回合,不清则面板滞留幽灵"在跑"项)
            for key in [k for k in agents if k[0] == sess]:
                agents[key].last_event = f"turn_end {ts[11:16]}"
                agents.pop(key)
    for sess, labels in first_order.items():
        active = latest.get(sess, [])
        for n, label in enumerate(labels):
            state = "active" if label in active else "done"   # 最新快照缺席 → done
            frag.tasks.append(Task(id=f"todo-{sess}-{n}", label=label,
                                   state=state, lane="会话",
                                   since=first_seen[(sess, label)], source="session"))
    frag.activity = list(agents.values())
    return frag


def _git(repo: Path, *args: str):
    r = subprocess.run(["git", "-C", str(repo), *args],
                       capture_output=True, text=True, timeout=5)
    if r.returncode != 0:
        raise RuntimeError(r.stderr.strip())
    return r.stdout


def _try_git(repo: Path, *args: str):
    """单条 git 探测失败(含超时/无 git)返回 None,由调用方决定字段级降级。"""
    try:
        return _git(repo, *args)
    except (RuntimeError, OSError, subprocess.SubprocessError):
        return None


def load_git(repo: Path, now_iso: str) -> Fragment:
    frag = Fragment(source="git")
    if _try_git(repo, "rev-parse", "--git-dir") is None:
        frag.warnings.append("git 源不可用")   # 非仓/无 git:空分片降级,不白屏
        return frag
    # 已确认是仓库;unborn HEAD(尚无首提交)只缺 branch/log,字段级降级,不整片清空
    branch = _try_git(repo, "rev-parse", "--abbrev-ref", "HEAD")
    if branch is not None:
        frag.velocity["branch"] = branch.strip()
    commits = _try_git(repo, "log", "--since=7.days", "--oneline")
    frag.velocity["commits_7d"] = 0 if commits is None else len(commits.splitlines())
    dirty = _try_git(repo, "status", "--porcelain")
    frag.velocity["dirty_files"] = 0 if dirty is None else len(dirty.splitlines())
    tags = _try_git(repo, "tag", "--sort=-creatordate") or ""
    for tag in tags.splitlines()[:5]:
        if tag:
            frag.milestones.append(Milestone(id=tag, state="done"))
    return frag


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


def _mtime(p: Path) -> float:
    """mtime 排序键守卫:不存在或 stat 失败一律 0(spec §9:glob 与 stat 之间文件可能消失)。"""
    try:
        return p.stat().st_mtime
    except OSError:
        return 0.0


def load_sdd(root: Path, now_iso: str):
    frag = Fragment(source="sdd")
    barriers = []
    # R12:按 mtime 取最新工作区——同日期多工作区时按路径名排序会选错"最新"
    workspaces = sorted((root / ".superpowers" / "sdd").glob("*/dag.json"), key=_mtime)
    if not workspaces:
        return frag, barriers
    parsed = []                  # [(milestone, tasks, barriers)] 旧→新;单源损坏只跳过
    for path in workspaces:      # 全部工作区 → 历史波次里程碑(spec §9:损坏源不白屏)
        try:
            dag = json.loads(path.read_text(encoding="utf-8"))
            ledger_p = path.parent / "progress.md"
            ledger = ledger_p.read_text(encoding="utf-8") if ledger_p.exists() else ""
            # R18:活跃任务 since=本工作区 progress.md mtime(「台账 2h 无跃迁→⚑」的
            # 近似语义);台账缺失时 since=None(不可判停,不虚报)
            try:
                ledger_since = (datetime.fromtimestamp(ledger_p.stat().st_mtime)
                                .astimezone().isoformat(timespec="seconds"))
            except OSError:
                ledger_since = None
            states = {n: _resolve(ledger, n) for n in (int(k) for k in dag["tasks"])}
            done = sum(1 for s, _ in states.values() if s == "done")
            active = any(s == "active" for s, _ in states.values())
            lane_of = {n: lane["name"] for lane in dag["lanes"] for n in lane["tasks"]}
            ms = Milestone(
                id=dag["wave"], title=dag.get("title", ""),
                state="active" if active else ("done" if done == len(states) else "planned"),
                tasks_done=done, tasks_total=len(states))
            ws_tasks = []
            for n in sorted(states):
                state, note = states[n]
                ws_tasks.append(Task(id=f"T{n}", label=dag["tasks"][str(n)]["label"],
                                     state=state, lane=lane_of.get(n, "无车道"),
                                     source="sdd", note=note,
                                     since=ledger_since if state == "active" else None))
            ws_barriers = []
            for b in dag.get("barriers", []):
                gate = "+".join(f"T{n}" for n in b["after"])
                unlocks = " ".join(f"T{n}" for n in b["unlocks"])
                ws_barriers.append(f"屏障 {b['id']}: {gate} → {unlocks}")
        # UnicodeDecodeError/JSONDecodeError 均为 ValueError 子类,显式列出以自文档;
        # ValueError 另兜住非数字任务键(int("abc"))——spec §9:损坏源跳过,不白屏
        except (json.JSONDecodeError, UnicodeDecodeError, ValueError,
                KeyError, TypeError, OSError):
            frag.warnings.append("sdd 源不可用")   # git+session 分片照常出面板
            continue
        parsed.append((ms, ws_tasks, ws_barriers))
    for i, (ms, ws_tasks, ws_barriers) in enumerate(parsed):
        frag.milestones.append(ms)
        if i == len(parsed) - 1:     # 最新"可解析"工作区:tasks/barriers 只出自它
            frag.tasks.extend(ws_tasks)
            barriers.extend(ws_barriers)
    return frag, barriers
