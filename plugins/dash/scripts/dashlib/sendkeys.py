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
