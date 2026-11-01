#!/usr/bin/env bash
# W80-E6B N-1 冻结签名基准封存脚本(一条命令复现)。
#
# 用仓库既有工具链,对四个参考包(2 个多组件 Application + 2 个用户态 Driver)
# 依次执行:nlos-package keygen/build/conformance/verify -> nlos-certify 四门认证
# -> n1-baseline-sealer(nlos-publisher-trust 三级证书链 + nlos-store-registry
# OCI 目录封存 Candidate->Stable + 封存清单落盘)。
#
# 用法:
#   bash examples/ecosystem-apps/seal-n1-baseline.sh
#
# 环境变量:
#   CARGO_TARGET_DIR  根工作区构建隔离目录(并行车道防锁竞争;默认 <repo>/target)
#   SEAL_QUIET=1      省略各包 CLI 明细输出
#
# 产物(全部入库,字节确定性):
#   examples/ecosystem-apps/<app>/sealed/<label>.v{1.0.0,1.1.0}.nlos-pkg
#   examples/ecosystem-apps/<app>/sealed/<label>.v1.0.0.certify.json
#   examples/ecosystem-apps/<app>/sealed/<label>.v1.0.0.store-seal.txt
#   examples/ecosystem-drivers/<driver>/sealed/...(同上)
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
APPS_WS="$REPO_ROOT/examples/ecosystem-apps"
DRIVERS_WS="$REPO_ROOT/examples/ecosystem-drivers"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
PKG_BIN="$CARGO_TARGET_DIR/debug/nlos-package$(case "$OS" in windows*) echo .exe;; esac)"
CERT_BIN="$CARGO_TARGET_DIR/debug/nlos-certify$(case "$OS" in windows*) echo .exe;; esac)"

say() { printf '[seal-script] %s\n' "$*"; }
run_detail() { if [ "${SEAL_QUIET:-0}" = "1" ]; then "$@" >/dev/null; else "$@"; fi }

say "1/5 building toolchain CLIs (nlos-package, nlos-certify) in CARGO_TARGET_DIR=$CARGO_TARGET_DIR"
(cd "$REPO_ROOT" && cargo build --locked -p nlos-package -p nlos-certify)

say "2/5 building the N-1 baseline sealer"
(cd "$APPS_WS" && cargo build --locked -p n1-baseline-sealer)
SEALER_BIN="$APPS_WS/target/debug/n1-baseline-sealer$(case "$OS" in windows*) echo .exe;; esac)"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

say "3/5 keygen + build + conformance for every frozen .nlos-pkg artifact"
mkdir -p \
  "$APPS_WS/knowledge-observatory-app/sealed" \
  "$APPS_WS/field-survey-app/sealed" \
  "$DRIVERS_WS/mcp-bridge-driver/sealed" \
  "$DRIVERS_WS/openai-compat-driver/sealed"
seal_one_tree() {
  local label="$1" tree="$2" version="$3" seed_hex="$4" sealed_dir="$5"
  local key="$WORK/$label.devkey"
  run_detail "$PKG_BIN" keygen --seed "$seed_hex" --out "$key"
  run_detail "$PKG_BIN" build "$tree" --key "$key" --out "$sealed_dir/$label.v$version.nlos-pkg"
  run_detail "$PKG_BIN" conformance "$sealed_dir/$label.v$version.nlos-pkg"
  run_detail "$PKG_BIN" verify "$sealed_dir/$label.v$version.nlos-pkg"
  sha256sum "$sealed_dir/$label.v$version.nlos-pkg"
}

# Deterministic dev signing seeds per package (sample-only; one byte repeated 32x).
hex_seed() { local out=""; for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32; do out+="$1"; done; printf '%s' "$out"; }
OBS_SEED="$(hex_seed 6b)"
FIELD_SEED="$(hex_seed 66)"
MCP_SEED="$(hex_seed 6d)"
OAI_SEED="$(hex_seed 6f)"

seal_one_tree knowledge-observatory-app "$APPS_WS/knowledge-observatory-app/package" 1.0.0 "$OBS_SEED" "$APPS_WS/knowledge-observatory-app/sealed"
seal_one_tree knowledge-observatory-app "$APPS_WS/knowledge-observatory-app/package-v1.1.0" 1.1.0 "$OBS_SEED" "$APPS_WS/knowledge-observatory-app/sealed"
seal_one_tree field-survey-app "$APPS_WS/field-survey-app/package" 1.0.0 "$FIELD_SEED" "$APPS_WS/field-survey-app/sealed"
seal_one_tree field-survey-app "$APPS_WS/field-survey-app/package-v1.1.0" 1.1.0 "$FIELD_SEED" "$APPS_WS/field-survey-app/sealed"
seal_one_tree mcp-bridge-driver "$DRIVERS_WS/mcp-bridge-driver/package" 1.0.0 "$MCP_SEED" "$DRIVERS_WS/mcp-bridge-driver/sealed"
seal_one_tree mcp-bridge-driver "$DRIVERS_WS/mcp-bridge-driver/package-v1.1.0" 1.1.0 "$MCP_SEED" "$DRIVERS_WS/mcp-bridge-driver/sealed"
seal_one_tree openai-compat-driver "$DRIVERS_WS/openai-compat-driver/package" 1.0.0 "$OAI_SEED" "$DRIVERS_WS/openai-compat-driver/sealed"
seal_one_tree openai-compat-driver "$DRIVERS_WS/openai-compat-driver/package-v1.1.0" 1.1.0 "$OAI_SEED" "$DRIVERS_WS/openai-compat-driver/sealed"

say "4/5 nlos-certify four-gate certification (text + JSON evidence)"
certify_one() {
  local label="$1" base="$2" upgrade="$3" sealed_dir="$4"
  run_detail "$CERT_BIN" certify --package-dir "$base" --upgrade-dir "$upgrade" --format text
  "$CERT_BIN" certify --package-dir "$base" --upgrade-dir "$upgrade" --format json \
    > "$sealed_dir/$label.v1.0.0.certify.json"
  grep -o '"verdict":"CERTIFIED"' "$sealed_dir/$label.v1.0.0.certify.json" | head -1
}
certify_one knowledge-observatory-app "$APPS_WS/knowledge-observatory-app/package" "$APPS_WS/knowledge-observatory-app/package-v1.1.0" "$APPS_WS/knowledge-observatory-app/sealed"
certify_one field-survey-app "$APPS_WS/field-survey-app/package" "$APPS_WS/field-survey-app/package-v1.1.0" "$APPS_WS/field-survey-app/sealed"
certify_one mcp-bridge-driver "$DRIVERS_WS/mcp-bridge-driver/package" "$DRIVERS_WS/mcp-bridge-driver/package-v1.1.0" "$DRIVERS_WS/mcp-bridge-driver/sealed"
certify_one openai-compat-driver "$DRIVERS_WS/openai-compat-driver/package" "$DRIVERS_WS/openai-compat-driver/package-v1.1.0" "$DRIVERS_WS/openai-compat-driver/sealed"

say "5/5 publisher PKI chain + OCI store sealing (Candidate -> Stable) + seal records"
"$SEALER_BIN" --repo-root "$REPO_ROOT" --work-dir "$WORK/sealer-state"

say "done: frozen N-1 baseline sealed under examples/ecosystem-{apps,drivers}/<label>/sealed/"
