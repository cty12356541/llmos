#!/bin/sh
# W33-B + W65-CAPP-PAYLOAD 第三方样板应用全生命周期演练（B1-5 / 移交 #2 C-APP-PAYLOAD）：
# 非内核视角开发 → keygen → build → conformance → verify → install → run（含载荷执行车道）
# → update → payload(gen 2) → uninstall，以及纯 `nlos-package` CLI 端到端闭环，
# 只骑公共面（`nlos-package` CLI + `sample-app-driver` 公共 API 驱动），零内核内部 API。
# 任何一步的退出码或输出断言失败即整体失败（exit != 0）。
#
# 用法：sh examples/sample-app/lifecycle.sh
# 可覆盖：NLOS_PACKAGE_BIN / DRIVER_BIN（缺省自动在各自 cargo 项目里构建）。

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd -- "$SCRIPT_DIR/../.." && pwd)
PACKAGE_ID=9a1c7d3e5f0b2a4c6d8e0f1a2b3c4d5e
EXECUTABLE_ENTRY=sample-driver
# 样板专用演示种子（生产签名密钥的托管是部署侧关注点，见 packaging.md §4）.
KEY_SEED=5a11e0b7c3d9f1426a8b0c5d7e93f618a2b4c6d8e0f2a4b6c8d0e2f4a6b8c0d1

WORK=$(mktemp -d "${TMPDIR:-/tmp}/sample-app-lifecycle.XXXXXX")
STATE=$WORK/state
CLI_STATE=$WORK/cli-state
LOGS=$WORK/logs
mkdir -p "$STATE" "$CLI_STATE" "$LOGS"
trap 'rm -rf "$WORK"' EXIT

say() { printf '[lifecycle] %s\n' "$*"; }

# 断言：日志文件里必须能 grep 到指定模式。
expect_line() {
    log=$1
    pattern=$2
    if ! grep -qE "$pattern" "$log"; then
        say "ASSERT FAILED: /$pattern/ not found in $log"
        sed -n '1,60p' "$log" || true
        exit 1
    fi
}

# 断言：命令必须以期望的非零退出码失败。
expect_exit() {
    expected_rc=$1
    shift
    set +e
    "$@"
    actual_rc=$?
    set -e
    if [ "$actual_rc" -ne "$expected_rc" ]; then
        say "ASSERT FAILED: expected exit $expected_rc, got $actual_rc for: $*"
        exit 1
    fi
}

# ---- 0. 工具链就位（可经环境变量覆盖；缺省就地构建） ----
if [ -z "${NLOS_PACKAGE_BIN:-}" ]; then
    say "building nlos-package (cargo build -p nlos-package)"
    (cd "$REPO_ROOT" && cargo build -q -p nlos-package)
    if [ -x "$REPO_ROOT/target/debug/nlos-package.exe" ]; then
        NLOS_PACKAGE_BIN=$REPO_ROOT/target/debug/nlos-package.exe
    else
        NLOS_PACKAGE_BIN=$REPO_ROOT/target/debug/nlos-package
    fi
fi
if [ -z "${DRIVER_BIN:-}" ]; then
    say "building sample-app-driver (cargo build in examples/sample-app)"
    (cd "$SCRIPT_DIR" && cargo build -q)
    if [ -x "$SCRIPT_DIR/target/debug/sample-app-driver.exe" ]; then
        DRIVER_BIN=$SCRIPT_DIR/target/debug/sample-app-driver.exe
    else
        DRIVER_BIN=$SCRIPT_DIR/target/debug/sample-app-driver
    fi
fi
command -v "$NLOS_PACKAGE_BIN" >/dev/null 2>&1 || { say "missing $NLOS_PACKAGE_BIN"; exit 1; }
command -v "$DRIVER_BIN" >/dev/null 2>&1 || { say "missing $DRIVER_BIN"; exit 1; }

# ---- 1. keygen：开发签名密钥（熵由开发者供给） ----
say "STEP 1 keygen"
"$NLOS_PACKAGE_BIN" keygen --seed "$KEY_SEED" --out "$WORK/sample-app.devkey" | tee "$LOGS/01-keygen.log"
expect_line "$LOGS/01-keygen.log" '^KEYGEN '
expect_line "$LOGS/01-keygen.log" '^principal '

# ---- 2. build + conformance v1.0.0：开发者目录 → 已签名自包含包文件 ----
say "STEP 2 build + conformance v1.0.0"
"$NLOS_PACKAGE_BIN" build "$SCRIPT_DIR/package" \
    --key "$WORK/sample-app.devkey" \
    --out "$WORK/sample-app-v1.0.0.nlospkg" | tee "$LOGS/02-build-v1.log"
expect_line "$LOGS/02-build-v1.log" '^BUILT '
expect_line "$LOGS/02-build-v1.log" 'entries 2 tasks 2'
"$NLOS_PACKAGE_BIN" conformance "$WORK/sample-app-v1.0.0.nlospkg" | tee "$LOGS/02-conformance-v1.log"
expect_line "$LOGS/02-conformance-v1.log" '^CONFORMANT '

# ---- 3. verify v1.0.0：与内核同一条权威验签路径，产出 receipt ----
say "STEP 3 verify v1.0.0 (authoritative pipeline -> durable receipt)"
"$NLOS_PACKAGE_BIN" verify "$WORK/sample-app-v1.0.0.nlospkg" \
    --store "$STATE/artifacts" --identity "$STATE/identity" | tee "$LOGS/03-verify-v1.log"
expect_line "$LOGS/03-verify-v1.log" '^VERIFIED '
RECEIPT_V1=$(sed -n 's/^VERIFIED //p' "$LOGS/03-verify-v1.log" | tr -d '\r')
[ -n "$RECEIPT_V1" ] || { say "empty v1.0.0 receipt id"; exit 1; }

# ---- 4. install：receipt digest-binding 衔接安装权威 ----
say "STEP 4 install (receipt -> ApplicationAuthority)"
"$DRIVER_BIN" install "$STATE" "$RECEIPT_V1" | tee "$LOGS/04-install.log"
# short_hex 只回前 8 字节（16 个十六进制字符）。
expect_line "$LOGS/04-install.log" 'generation=1'
expect_line "$LOGS/04-install.log" "package=$(printf %.16s "$PACKAGE_ID")"

# ---- 5. run：载荷执行车道（app.bin）+ task/attempt/operation + 应用注册 + Os 服务替身 ----
say "STEP 5 run (payload execution lane + task -> attempt -> process -> fiber -> commit)"
"$DRIVER_BIN" run "$STATE" "$PACKAGE_ID" | tee "$LOGS/05-run.log"
expect_line "$LOGS/05-run.log" "PAYLOAD entry=$EXECUTABLE_ENTRY payload_operation_id=[0-9a-f]{32} payload_generation=1 payload_digest=[0-9a-f]{64}"
expect_line "$LOGS/05-run.log" 'kind=payload-execution'
expect_line "$LOGS/05-run.log" 'associated_application=true'
expect_line "$LOGS/05-run.log" 'kind=commit-permit'
expect_line "$LOGS/05-run.log" 'kind=task-commit'
expect_line "$LOGS/05-run.log" 'output_head_revision=2'
expect_line "$LOGS/05-run.log" 'outstanding_tasks=1'
GEN1_OP_ID=$(sed -n 's/.*payload_operation_id=\([0-9a-f]\{32\}\).*/\1/p' "$LOGS/05-run.log" | head -n 1 | tr -d '\r')
GEN1_DIGEST=$(sed -n 's/.*payload_digest=\([0-9a-f]\{64\}\).*/\1/p' "$LOGS/05-run.log" | head -n 1 | tr -d '\r')
SERVICE_PID=$(sed -n 's/.*service_os_pid=\([0-9][0-9]*\).*/\1/p' "$LOGS/05-run.log" | tail -1 | tr -d '\r')
[ -n "$GEN1_OP_ID" ] || { say "run produced no payload_operation_id"; exit 1; }
[ -n "$GEN1_DIGEST" ] || { say "run produced no payload_digest"; exit 1; }
[ -n "$SERVICE_PID" ] || { say "run produced no service pid"; exit 1; }

# 同一持久 $STATE 上经 `nlos-package run` 复核：必须精确 replay 同一笔代际 1 载荷执行回执。
say "STEP 5b nlos-package run replay on generation 1"
"$NLOS_PACKAGE_BIN" run "$PACKAGE_ID" "$EXECUTABLE_ENTRY" --root "$STATE" | tee "$LOGS/05b-cli-run-gen1.log"
expect_line "$LOGS/05b-cli-run-gen1.log" '^decision replayed$'
expect_line "$LOGS/05b-cli-run-gen1.log" "^operation $GEN1_OP_ID$"
expect_line "$LOGS/05b-cli-run-gen1.log" "digest $GEN1_DIGEST"

# ---- 6. build + conformance + verify v1.1.0：同 major 新 revision ----
say "STEP 6 build + conformance + verify v1.1.0 (same-major update)"
"$NLOS_PACKAGE_BIN" build "$SCRIPT_DIR/package-v1.1.0" \
    --key "$WORK/sample-app.devkey" \
    --out "$WORK/sample-app-v1.1.0.nlospkg" | tee "$LOGS/06-build-v2.log"
expect_line "$LOGS/06-build-v2.log" '^BUILT '
"$NLOS_PACKAGE_BIN" conformance "$WORK/sample-app-v1.1.0.nlospkg" | tee "$LOGS/06-conformance-v2.log"
expect_line "$LOGS/06-conformance-v2.log" '^CONFORMANT '
"$NLOS_PACKAGE_BIN" verify "$WORK/sample-app-v1.1.0.nlospkg" \
    --store "$STATE/artifacts" --identity "$STATE/identity" | tee "$LOGS/06-verify-v2.log"
expect_line "$LOGS/06-verify-v2.log" '^VERIFIED '
RECEIPT_V2=$(sed -n 's/^VERIFIED //p' "$LOGS/06-verify-v2.log" | tr -d '\r')
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

# 代际 2 载荷执行：更新后的 v1.1.0 `app.bin` 字节经 `nlos-package run` 产出全新 operation 与摘要。
say "STEP 7b nlos-package run on generation 2 (updated v1.1.0 payload)"
"$NLOS_PACKAGE_BIN" run "$PACKAGE_ID" "$EXECUTABLE_ENTRY" --root "$STATE" | tee "$LOGS/07b-cli-run-gen2.log"
expect_line "$LOGS/07b-cli-run-gen2.log" '^decision executed$'
expect_line "$LOGS/07b-cli-run-gen2.log" 'generation 2'
GEN2_OP_ID=$(sed -n 's/^operation \([0-9a-f]\{32\}\)$/\1/p' "$LOGS/07b-cli-run-gen2.log" | tr -d '\r')
GEN2_DIGEST=$(sed -n 's/.*digest \([0-9a-f]\{64\}\) bytes.*/\1/p' "$LOGS/07b-cli-run-gen2.log" | tr -d '\r')
[ "$GEN1_OP_ID" != "$GEN2_OP_ID" ] || { say "generation 2 must derive a distinct operation id"; exit 1; }
[ "$GEN1_DIGEST" != "$GEN2_DIGEST" ] || { say "generation 2 must consume updated v1.1.0 app.bin bytes"; exit 1; }

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
say "service stand-in pid $SERVICE_PID is gone"

# ---- 9. 负门：卸载之后 driver run（exit 2）与 nlos-package run（exit 7）均必须 fail-closed ----
say "STEP 9 negative gate: run after uninstall must fail closed"
expect_exit 2 "$DRIVER_BIN" run "$STATE" "$PACKAGE_ID" >"$LOGS/09-run-after-uninstall.log" 2>&1
expect_line "$LOGS/09-run-after-uninstall.log" 'is not installed \(status=uninstalled\)'
expect_exit 7 "$NLOS_PACKAGE_BIN" run "$PACKAGE_ID" "$EXECUTABLE_ENTRY" --root "$STATE" >"$LOGS/09b-cli-run-after-uninstall.log" 2>&1
expect_line "$LOGS/09b-cli-run-after-uninstall.log" 'not installed'

# ---- 10. 纯 CLI 消费面闭环（独立 $CLI_STATE：install -> run -> update -> run -> uninstall） ----
say "STEP 10 pure nlos-package CLI lifecycle (install -> run -> update -> run -> uninstall)"
"$NLOS_PACKAGE_BIN" install "$WORK/sample-app-v1.0.0.nlospkg" --root "$CLI_STATE" | tee "$LOGS/10a-cli-install.log"
expect_line "$LOGS/10a-cli-install.log" '^decision installed$'
expect_line "$LOGS/10a-cli-install.log" 'generation 1'
expect_line "$LOGS/10a-cli-install.log" "^executables $EXECUTABLE_ENTRY$"

"$NLOS_PACKAGE_BIN" run "$PACKAGE_ID" "$EXECUTABLE_ENTRY" --root "$CLI_STATE" | tee "$LOGS/10b-cli-run-v1.log"
expect_line "$LOGS/10b-cli-run-v1.log" '^decision executed$'
expect_line "$LOGS/10b-cli-run-v1.log" 'generation 1'
expect_line "$LOGS/10b-cli-run-v1.log" "digest $GEN1_DIGEST"

expect_exit 4 "$NLOS_PACKAGE_BIN" run "$PACKAGE_ID" "assets/config.bin" --root "$CLI_STATE" >"$LOGS/10c-cli-non-exec.log" 2>&1
expect_line "$LOGS/10c-cli-non-exec.log" 'entry artifact unknown:'

"$NLOS_PACKAGE_BIN" update "$WORK/sample-app-v1.1.0.nlospkg" --root "$CLI_STATE" --compat same-major | tee "$LOGS/10d-cli-update.log"
expect_line "$LOGS/10d-cli-update.log" '^decision updated$'
expect_line "$LOGS/10d-cli-update.log" 'generation 2'
expect_line "$LOGS/10d-cli-update.log" '^compat same-major$'

"$NLOS_PACKAGE_BIN" run "$PACKAGE_ID" "$EXECUTABLE_ENTRY" --root "$CLI_STATE" | tee "$LOGS/10e-cli-run-v2.log"
expect_line "$LOGS/10e-cli-run-v2.log" '^decision executed$'
expect_line "$LOGS/10e-cli-run-v2.log" 'generation 2'
expect_line "$LOGS/10e-cli-run-v2.log" "digest $GEN2_DIGEST"

"$NLOS_PACKAGE_BIN" uninstall "$PACKAGE_ID" --root "$CLI_STATE" | tee "$LOGS/10f-cli-uninstall.log"
expect_line "$LOGS/10f-cli-uninstall.log" '^decision uninstalled$'
expect_line "$LOGS/10f-cli-uninstall.log" 'generation 2'

expect_exit 7 "$NLOS_PACKAGE_BIN" run "$PACKAGE_ID" "$EXECUTABLE_ENTRY" --root "$CLI_STATE" >"$LOGS/10g-cli-post-uninstall.log" 2>&1

say "LIFECYCLE OK (keygen -> build -> conformance -> verify -> install -> run/payload -> update -> uninstall)"
