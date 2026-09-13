# plugins/dash/scripts/dashlib/adapters.py
"""适配器层:session(事件重放)/ git(快照派生)/ sdd(台账深语义)。

v1 简化注记:todo 快照 summary 只带 in_progress 项(R10 起,空串=无在途),
以标签身份跨快照配对——最新快照存在即 active,缺席即 done。
"""
from __future__ import annotations

import json
import re
import subprocess
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
            for key in agents:
                if key[0] == sess:
                    agents[key].last_event = f"turn_end {ts[11:16]}"
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


def load_sdd(root: Path, now_iso: str):
    frag = Fragment(source="sdd")
    barriers = []
    # R12:按 mtime 取最新工作区——同日期多工作区时按路径名排序会选错"最新"
    workspaces = sorted((root / ".superpowers" / "sdd").glob("*/dag.json"),
                        key=lambda p: p.stat().st_mtime)
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
