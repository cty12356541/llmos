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
    except (OSError, ValueError, KeyError, TypeError):
        # TypeError:合法 JSON 但非对象(如 "T9")→ 字符串下标取值即抛;视为无焦
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
