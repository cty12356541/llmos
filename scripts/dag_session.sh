#!/bin/sh
# 一键"左侧 Claude + 右侧常驻 DAG"tmux 布局(方案 A 终态)。
# 用法:sh scripts/dag_session.sh [右侧宽度百分比,默认 28]
# 需要:tmux(brew install tmux)。重复运行会复用已存在的 dag 会话。
# 需要插件技能/hook 时,用 claude --plugin-dir plugins/dash 启动左栏会话
set -e
REPO="$(cd "$(dirname "$0")/.." && pwd)"
SESSION="llmos-dag"
WIDTH="${1:-28}"

if ! command -v tmux >/dev/null 2>&1; then
    echo "未安装 tmux:先跑 brew install tmux" >&2
    exit 1
fi

if tmux has-session -t "$SESSION" 2>/dev/null; then
    echo "复用已有会话 $SESSION"
    exec tmux attach-session -t "$SESSION"
fi

# 左栏 100% 起步 → 右侧切出 WIDTH% 跑 DAG 面板;左栏留在项目根供启动 claude
tmux new-session -d -s "$SESSION" -c "$REPO" -x 200 -y 50
tmux split-window -h -p "$WIDTH" -t "$SESSION" -c "$REPO" \
    "python3 $REPO/plugins/dash/scripts/dash watch; exec sh"
tmux select-pane -t "$SESSION".0
tmux set-option -t "$SESSION" remain-on-exit on >/dev/null 2>&1 || true

echo "布局就绪:左栏=项目根(跑 claude),右栏=常驻 DAG"
exec tmux attach-session -t "$SESSION"
