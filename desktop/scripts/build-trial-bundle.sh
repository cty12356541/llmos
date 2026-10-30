#!/usr/bin/env sh
# Build or preflight-check an unsigned local trial desktop bundle for llmos-task-manager
# (Stage E E0 / W77-PRE-C).
#
# Modes:
#   --check-only / -CheckOnly : Fast environment and configuration gate (exit 0 without heavy builds).
#   (no arguments)            : Full local trial bundle build (`npm ci`/`npm install`, `npm run build`,
#                               `npx tauri build --no-sign --config <temp_override_json>`).
#
# Honest boundary:
#   本产物仅限本机开发试用，未经过 Apple Developer ID / Microsoft EV 硬件令牌商业 CA 公证签名，
#   商业发行签名维持在 E-deferred / RISK-B-06。

set -eu

CHECK_ONLY=0
for arg in "$@"; do
    case "$arg" in
        --check-only|-CheckOnly|-checkonly)
            CHECK_ONLY=1
            ;;
        *)
            echo "[build-trial-bundle] Unknown argument: $arg. Usage: build-trial-bundle.sh [--check-only]" >&2
            exit 1
            ;;
    esac
done

BOUNDARY_BANNER="本产物仅限本机开发试用，未经过 Apple Developer ID / Microsoft EV 硬件令牌商业 CA 公证签名，商业发行签名维持在 E-deferred / RISK-B-06"

echo "================================================================================"
echo "[build-trial-bundle] 诚实边界声明：${BOUNDARY_BANNER}"
echo "================================================================================"

SCRIPT_DIR="$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)"
DESKTOP_DIR="$(CDPATH= cd -- "${SCRIPT_DIR}/.." && pwd)"
REPO_ROOT="$(CDPATH= cd -- "${SCRIPT_DIR}/../.." && pwd)"

require_cmd() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "[build-trial-bundle] Required tool '$1' was not found in PATH." >&2
        exit 1
    fi
}

require_cmd node
require_cmd npm
require_cmd cargo
require_cmd rustc

NODE_VERSION_RAW="$(node --version | tr -d '\r')"
NODE_MAJOR="$(printf '%s' "$NODE_VERSION_RAW" | sed -e 's/^v//' -e 's/\..*$//')"
case "$NODE_MAJOR" in
    ''|*[!0-9]*)
        echo "[build-trial-bundle] Unable to parse Node.js major version from '${NODE_VERSION_RAW}'." >&2
        exit 1
        ;;
esac
if [ "$NODE_MAJOR" -lt 20 ]; then
    echo "[build-trial-bundle] Node.js >= 20 is required by desktop/package.json (detected ${NODE_VERSION_RAW})." >&2
    exit 1
fi

NPM_VERSION_RAW="$(npm --version | tr -d '\r')"
CARGO_VERSION_RAW="$(cargo --version | tr -d '\r')"
RUSTC_VERSION_RAW="$(rustc --version | tr -d '\r')"

PACKAGE_JSON="${DESKTOP_DIR}/package.json"
TAURI_CONF="${DESKTOP_DIR}/src-tauri/tauri.conf.json"
TAURI_CARGO="${DESKTOP_DIR}/src-tauri/Cargo.toml"
ICON_PNG="${DESKTOP_DIR}/src-tauri/icons/icon.png"
ICON_ICO="${DESKTOP_DIR}/src-tauri/icons/icon.ico"

for required_file in "$PACKAGE_JSON" "$TAURI_CONF" "$TAURI_CARGO" "$ICON_PNG" "$ICON_ICO"; do
    if [ ! -f "$required_file" ]; then
        echo "[build-trial-bundle] Missing required desktop project file: ${required_file}" >&2
        exit 1
    fi
done

META_LINE="$(node -e '
const fs = require("fs");
const pkg = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
const conf = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
if (!pkg.scripts || !pkg.scripts.build) {
  console.error("missing scripts.build in desktop/package.json");
  process.exit(2);
}
const cli = pkg.devDependencies && pkg.devDependencies["@tauri-apps/cli"];
if (!cli) {
  console.error("missing @tauri-apps/cli in desktop/package.json");
  process.exit(2);
}
if (!conf.productName || !conf.identifier) {
  console.error("missing productName or identifier in tauri.conf.json");
  process.exit(2);
}
process.stdout.write(`${conf.productName} (${conf.version || "0.0.0"}) | tauri_cli=${cli}`);
' "$PACKAGE_JSON" "$TAURI_CONF")"

echo "[build-trial-bundle] Environment verified:"
echo "  repo_root    = ${REPO_ROOT}"
echo "  desktop_dir  = ${DESKTOP_DIR}"
echo "  product      = ${META_LINE}"
echo "  node         = ${NODE_VERSION_RAW}"
echo "  npm          = ${NPM_VERSION_RAW}"
echo "  cargo        = ${CARGO_VERSION_RAW}"
echo "  rustc        = ${RUSTC_VERSION_RAW}"

if [ "$CHECK_ONLY" -eq 1 ]; then
    echo "[build-trial-bundle] CHECK_ONLY_OK (preflight passed; skipping npm build and tauri bundle)"
    echo "[build-trial-bundle] 诚实边界复核：${BOUNDARY_BANNER}"
    exit 0
fi

TEMP_OVERRIDE_CONFIG="${TMPDIR:-/tmp}/llmos-trial-tauri-override-$$.json"
cleanup() {
    rm -f "$TEMP_OVERRIDE_CONFIG"
}
trap cleanup EXIT INT TERM

printf '%s\n' '{"bundle":{"active":true,"icon":["icons/icon.png","icons/icon.ico"]}}' > "$TEMP_OVERRIDE_CONFIG"

cd "$DESKTOP_DIR"
if [ ! -d "${DESKTOP_DIR}/node_modules" ]; then
    if [ -f "${DESKTOP_DIR}/package-lock.json" ]; then
        echo "[build-trial-bundle] Installing frontend dependencies via npm ci..."
        npm ci --no-audit --no-fund
    else
        echo "[build-trial-bundle] Installing frontend dependencies via npm install..."
        npm install --no-audit --no-fund
    fi
fi

echo "[build-trial-bundle] Running frontend TypeScript check and Vite production bundle (npm run build)..."
npm run build

echo "[build-trial-bundle] Building local unsigned trial desktop bundle (npx tauri build --no-sign)..."
npx tauri build --no-sign --config "$TEMP_OVERRIDE_CONFIG"

BUNDLE_OUT_DIR="${DESKTOP_DIR}/src-tauri/target/release/bundle"
echo "================================================================================"
echo "[build-trial-bundle] TRIAL_BUNDLE_OK output_dir=${BUNDLE_OUT_DIR}"
echo "[build-trial-bundle] 诚实边界声明：${BOUNDARY_BANNER}"
echo "================================================================================"
