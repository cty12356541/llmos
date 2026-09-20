#!/bin/sh
# 常驻 DAG 面板(B 方案):清屏 → 只读渲染 → 睡 N 秒循环。
# 用法:sh scripts/dag_watch.sh [间隔秒数,默认 5]
# --view 只读出口:零文件写副作用,不与 integrator 的台账追加竞争。
# 建议开独立终端窗口跑本脚本,窗口拖到屏幕右侧(macOS 拖到边缘自动半屏吸附)。
INTERVAL="${1:-5}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"
while true; do
    clear
    if python3 "$REPO/scripts/render_dag.py" --view; then
        printf '\n[常驻面板] 每 %ss 自动刷新 · Ctrl-C 退出 · 数据源:SDD 台账\n' "$INTERVAL"
    else
        printf '\n[常驻面板] 当前无活跃波次工作区(.superpowers/sdd/*/dag.json)\n'
        printf '新波次启动后此处自动出现车道图;Ctrl-C 退出\n'
    fi
    sleep "$INTERVAL"
done
