#!/bin/sh
# W33-B 第三方样板应用全生命周期演练（B1-5）：
# 非内核视角开发 → sign → install → run → update → uninstall，
# 只骑公共面（`nlos-package` CLI + `sample-app-driver` 公共 API 驱动），
# 零内核内部 API。任何一步的退出码或输出断言失败即整体失败（exit ≠ 0）。
#
# 用法：sh examples/sample-app/lifecycle.sh
# 可覆盖：NLOS_PACKAGE_BIN / DRIVER_BIN（缺省自动在各自 cargo 项目里构建）。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)
PACKAGE_ID=9a1c7d3e5f0b2a4c6d8e0f1a2b3c4d5e
# 样板专用演示种子（生产签名密钥的托管是部署侧关注点，见 packaging.md §4）.
KEY_SEED=5a11e0b7c3d9f1426a8b0c5d7e93f618a2b4c6d8e0f2a4b6c8d0e2f4a6b8c0d1

WORK=$(mktemp -d "${TMPDIR:-/tmp}/sample-app-lifecycle.XXXXXX")
STATE=$WORK/state
LOGS=$WORK/logs
mkdir -p "$STATE" "$LOGS"
trap 'rm -rf "$WORK"' EXIT

say() { printf '[lifecycle] %s\n' "$*"; }

# 断言：日志文件里必须能 grep 到指定模式。
expect_line() {
    log=$1
    pattern=$2
    if ! grep -qE "$pattern" "$log"; then
        say "ASSERT FAILED: /$pattern/ not found in $log"
        sed -n '1,40p' "$log" || true
        exit 1
    fi
}

# ---- 0. 工具链就位（可经环境变量覆盖；缺省就地构建） ----
if [ -z "${NLOS_PACKAGE_BIN:-}" ]; then
    say "building nlos-package (cargo build -p nlos-package)"
    (cd "$REPO_ROOT" && cargo build -q -p nlos-package)
    NLOS_PACKAGE_BIN=$REPO_ROOT/target/debug/nlos-package
fi
if [ -z "${DRIVER_BIN:-}" ]; then
    say "building sample-app-driver (cargo build in examples/sample-app)"
    (cd "$SCRIPT_DIR" && cargo build -q)
    DRIVER_BIN=$SCRIPT_DIR/target/debug/sample-app-driver
fi
command -v "$NLOS_PACKAGE_BIN" >/dev/null 2>&1 || { say "missing $NLOS_PACKAGE_BIN"; exit 1; }
command -v "$DRIVER_BIN" >/dev/null 2>&1 || { say "missing $DRIVER_BIN"; exit 1; }

# ---- 1. keygen：开发签名密钥（熵由开发者供给） ----
say "STEP 1 keygen"
"$NLOS_PACKAGE_BIN" keygen --seed "$KEY_SEED" --out "$WORK/sample-app.devkey" | tee "$LOGS/01-keygen.log"
expect_line "$LOGS/01-keygen.log" '^KEYGEN '
expect_line "$LOGS/01-keygen.log" '^principal '

# ---- 2. build v1.0.0：开发者目录 → 已签名自包含包文件 ----
say "STEP 2 build v1.0.0"
"$NLOS_PACKAGE_BIN" build "$SCRIPT_DIR/package" \
    --key "$WORK/sample-app.devkey" \
    --out "$WORK/sample-app-v1.0.0.nlospkg" | tee "$LOGS/02-build-v1.log"
expect_line "$LOGS/02-build-v1.log" '^BUILT '
expect_line "$LOGS/02-build-v1.log" "entries 2 tasks 2"

# ---- 3. verify v1.0.0：与内核同一条权威验签路径，产出 receipt ----
say "STEP 3 verify v1.0.0 (authoritative pipeline -> durable receipt)"
"$NLOS_PACKAGE_BIN" verify "$WORK/sample-app-v1.0.0.nlospkg" \
    --store "$STATE/artifacts" --identity "$STATE/identity" | tee "$LOGS/03-verify-v1.log"
expect_line "$LOGS/03-verify-v1.log" '^VERIFIED '
RECEIPT_V1=$(sed -n 's/^VERIFIED //p' "$LOGS/03-verify-v1.log")
[ -n "$RECEIPT_V1" ] || { say "empty v1.0.0 receipt id"; exit 1; }

# ---- 4. install：receipt digest-binding 衔接安装权威 ----
say "STEP 4 install (receipt -> ApplicationAuthority)"
"$DRIVER_BIN" install "$STATE" "$RECEIPT_V1" | tee "$LOGS/04-install.log"
# short_hex 只回前 8 字节（16 个十六进制字符）。
expect_line "$LOGS/04-install.log" 'generation=1'
expect_line "$LOGS/04-install.log" "package=$(printf %.16s "$PACKAGE_ID")"


# ---- 5. run：task/attempt/operation 全走公共面 + 应用注册 + Os 服务替身 ----
say "STEP 5 run (task -> attempt -> process -> fiber operation -> commit)"
"$DRIVER_BIN" run "$STATE" "$PACKAGE_ID" | tee "$LOGS/05-run.log"
expect_line "$LOGS/05-run.log" 'associated_application=true'
expect_line "$LOGS/05-run.log" 'kind=commit-permit'
expect_line "$LOGS/05-run.log" 'kind=task-commit'
expect_line "$LOGS/05-run.log" 'output_head_revision=2'
expect_line "$LOGS/05-run.log" 'outstanding_tasks=1'
SERVICE_PID=$(sed -n 's/.*service_os_pid=\([0-9][0-9]*\).*/\1/p' "$LOGS/05-run.log" | tail -1)
[ -n "$SERVICE_PID" ] || { say "run produced no service pid"; exit 1; }

# ---- 6. build + verify v1.1.0：同 major 新 revision ----
say "STEP 6 build + verify v1.1.0 (same-major update)"
"$NLOS_PACKAGE_BIN" build "$SCRIPT_DIR/package-v1.1.0" \
    --key "$WORK/sample-app.devkey" \
    --out "$WORK/sample-app-v1.1.0.nlospkg" | tee "$LOGS/06-build-v2.log"
expect_line "$LOGS/06-build-v2.log" '^BUILT '
"$NLOS_PACKAGE_BIN" verify "$WORK/sample-app-v1.1.0.nlospkg" \
    --store "$STATE/artifacts" --identity "$STATE/identity" | tee "$LOGS/06-verify-v2.log"
expect_line "$LOGS/06-verify-v2.log" '^VERIFIED '
RECEIPT_V2=$(sed -n 's/^VERIFIED //p' "$LOGS/06-verify-v2.log")
[ -n "$RECEIPT_V2" ] || { say "empty v1.1.0 receipt id"; exit 1; }
[ "$RECEIPT_V1" != "$RECEIPT_V2" ] || { say "v1.1.0 must verify to a distinct receipt"; exit 1; }

# ---- 7. update：W29-E 迁移 runner（SameMajor 窗 + 步骤 + 健康检查 + 原子切换） ----
say "STEP 7 update (migration runner: begin -> steps -> health -> atomic switch)"
"$DRIVER_BIN" update "$STATE" "$PACKAGE_ID" "$RECEIPT_V2" | tee "$LOGS/07-update.log"
expect_line "$LOGS/07-update.log" 'drill=started'
expect_line "$LOGS/07-update.log" 'step=1 recorded'
expect_line "$LOGS/07-update.log" 'step=2 recorded'
expect_line "$LOGS/07-update.log" 'health passed=true'
expect_line "$LOGS/07-update.log" 'generation=2'
expect_line "$LOGS/07-update.log" 'application_status=installed'

# ---- 8. uninstall：W27-D 活动门拒绝在先，W30-D teardown 链过门在后 ----
say "STEP 8 uninstall (activity gate refusal -> W30-D teardown -> gated uninstall)"
"$DRIVER_BIN" uninstall "$STATE" "$PACKAGE_ID" "$SERVICE_PID" | tee "$LOGS/08-uninstall.log"
expect_line "$LOGS/08-uninstall.log" 'GATE_REFUSED active_task_count=1'
expect_line "$LOGS/08-uninstall.log" 'kills=1 crashes=1 linkages=1 task_cancels=1'
expect_line "$LOGS/08-uninstall.log" 'kill#0 decision=signaled'
expect_line "$LOGS/08-uninstall.log" 'status=uninstalled'

# Os 服务替身确实死了（Unix：平台 kill 链真实发过信号）。
if kill -0 "$SERVICE_PID" 2>/dev/null; then
    attempt=0
    while kill -0 "$SERVICE_PID" 2>/dev/null && [ "$attempt" -lt 10 ]; do
        attempt=$((attempt + 1))
        sleep 1
    done
    if kill -0 "$SERVICE_PID" 2>/dev/null; then
        say "ASSERT FAILED: service stand-in pid $SERVICE_PID survived the platform kill"
        exit 1
    fi
fi
say "service stand-in pid $SERVICE_PID is gone (platform kill was real)"

# ---- 9. 负门：卸载之后 run 必须失败（typed 拒绝，exit 2） ----
say "STEP 9 negative gate: run after uninstall must fail closed"
if "$DRIVER_BIN" run "$STATE" "$PACKAGE_ID" >"$LOGS/09-run-after-uninstall.log" 2>&1; then
    say "ASSERT FAILED: run unexpectedly succeeded on an uninstalled application"
    exit 1
fi
expect_line "$LOGS/09-run-after-uninstall.log" 'is not installed \(status=uninstalled\)'

say "LIFECYCLE OK (keygen -> build -> verify -> install -> run -> update -> uninstall)"
