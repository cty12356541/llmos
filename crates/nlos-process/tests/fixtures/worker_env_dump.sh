#!/bin/sh
# W82-C 受管 worker 环境白名单夹具:$1 = 输出基名(无空白路径)
env > "$1.env"
echo $$ > "$1.pid"
sleep 300 &
echo $! > "$1.grandpid"
wait
