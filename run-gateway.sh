#!/usr/bin/env bash
set -euo pipefail

# Build `agy-acp-gateway` in release mode (plus embedded web UI if Bun is installed) and start the server.
#
# Usage:
#   ./run-gateway.sh                 # Build release & run in foreground (Ctrl+C to stop)
#   ./run-gateway.sh --daemon        # Build release & run in background (.agy-runtime/gateway.pid)
#   ./run-gateway.sh --stop          # Stop background daemon
#   ./run-gateway.sh --listen 127.0.0.1:8787 --default-model gemini-4-carbon-high --api-key local-agy

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNTIME_DIR="${SCRIPT_DIR}/.agy-runtime"
mkdir -p "${RUNTIME_DIR}"

PID_FILE="${RUNTIME_DIR}/gateway.pid"
LOG_OUT="${RUNTIME_DIR}/gateway.out.log"
LOG_ERR="${RUNTIME_DIR}/gateway.err.log"

DAEMON=0
STOP=0
SKIP_BUILD=0
REBUILD_WEB=0
HAS_LISTEN=0
HAS_DEFAULT_MODEL=0
HAS_ACP_MODE=0
LISTEN_ADDR="${AGY_GATEWAY_LISTEN:-127.0.0.1:8787}"
DEFAULT_MODEL="${AGY_DEFAULT_MODEL:-gemini-4-carbon-high}"
ACP_MODE="${AGY_ACP_MODE:-auto}"
FORWARD_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --daemon|-Daemon)
      DAEMON=1
      shift
      ;;
    --stop|-Stop)
      STOP=1
      shift
      ;;
    --skip-build|-SkipBuild)
      SKIP_BUILD=1
      shift
      ;;
    --rebuild-web|-RebuildWeb)
      REBUILD_WEB=1
      shift
      ;;
    --listen)
      HAS_LISTEN=1
      FORWARD_ARGS+=("$1")
      shift
      if [[ $# -gt 0 ]]; then
        LISTEN_ADDR="$1"
        FORWARD_ARGS+=("$1")
        shift
      fi
      ;;
    --listen=*)
      HAS_LISTEN=1
      LISTEN_ADDR="${1#--listen=}"
      FORWARD_ARGS+=("$1")
      shift
      ;;
    --default-model|--default-model=*)
      HAS_DEFAULT_MODEL=1
      FORWARD_ARGS+=("$1")
      shift
      ;;
    --acp-mode|--acp-mode=*)
      HAS_ACP_MODE=1
      FORWARD_ARGS+=("$1")
      shift
      ;;
    *)
      FORWARD_ARGS+=("$1")
      shift
      ;;
  esac
done

if [[ "${STOP}" -eq 1 ]]; then
  if [[ -f "${PID_FILE}" ]]; then
    pid="$(cat "${PID_FILE}")"
    kill "${pid}" 2>/dev/null || true
    rm -f "${PID_FILE}"
    echo "[STOP] Stopped agy-acp-gateway (PID ${pid})."
  else
    echo "[INFO] No background gateway PID file found at ${PID_FILE}"
  fi
  exit 0
fi

if [[ "${SKIP_BUILD}" -eq 0 ]]; then
  if [[ "${REBUILD_WEB}" -eq 1 || ! -f "${SCRIPT_DIR}/web/dist/index.html" ]]; then
    if command -v bun >/dev/null 2>&1; then
      echo "[1/3] Building companion web dashboard (web/dist) ..."
      bun install --cwd "${SCRIPT_DIR}/web" --frozen-lockfile
      bun run --cwd "${SCRIPT_DIR}/web" build
    else
      echo "[1/3] bun not found; skipping web/dist build (API endpoints unaffected)."
    fi
  else
    echo "[1/3] Companion web bundle (web/dist) already built."
  fi

  echo "[2/3] Building release binary (cargo build --release --locked -p agy-acp-gateway) ..."
  (cd "${SCRIPT_DIR}" && cargo build --release --locked -p agy-acp-gateway)
fi

GW_BIN="${SCRIPT_DIR}/target/release/agy-acp-gateway"
if [[ ! -x "${GW_BIN}" ]]; then
  echo "[ERROR] Release binary not found at ${GW_BIN}" >&2
  exit 1
fi

FINAL_ARGS=()
if [[ "${HAS_LISTEN}" -eq 0 ]]; then
  FINAL_ARGS+=("--listen" "${LISTEN_ADDR}")
fi
if [[ "${HAS_DEFAULT_MODEL}" -eq 0 ]]; then
  FINAL_ARGS+=("--default-model" "${DEFAULT_MODEL}")
fi
if [[ "${HAS_ACP_MODE}" -eq 0 ]]; then
  FINAL_ARGS+=("--acp-mode" "${ACP_MODE}")
fi
if [[ ${#FORWARD_ARGS[@]} -gt 0 ]]; then
  FINAL_ARGS+=("${FORWARD_ARGS[@]}")
fi

BASE_URL="http://${LISTEN_ADDR}"
echo ""
echo "  Gateway Base & Web UI   : ${BASE_URL}/"
echo "  OpenAI Responses API    : ${BASE_URL}/v1/responses"
echo "  Anthropic Messages API  : ${BASE_URL}/v1/messages"
echo "  Live Models & Catalog   : ${BASE_URL}/v1/models  |  ${BASE_URL}/v1/codex-models"
echo ""

if [[ "${DAEMON}" -eq 1 ]]; then
  echo "[3/3] Starting agy-acp-gateway in background ..."
  nohup "${GW_BIN}" "${FINAL_ARGS[@]}" >"${LOG_OUT}" 2>"${LOG_ERR}" &
  gw_pid=$!
  echo "${gw_pid}" >"${PID_FILE}"

  ready=0
  for _ in {1..40}; do
    if ! kill -0 "${gw_pid}" 2>/dev/null; then
      echo "[ERROR] agy-acp-gateway exited unexpectedly:" >&2
      tail -n 20 "${LOG_ERR}" >&2 || true
      exit 1
    fi
    if curl -fsS "${BASE_URL}/healthz" >/dev/null 2>&1; then
      ready=1
      break
    fi
    sleep 0.3
  done

  if [[ "${ready}" -ne 1 ]]; then
    echo "[ERROR] Timed out waiting for ${BASE_URL}/healthz" >&2
    exit 1
  fi
  echo "[OK] Gateway daemon running (PID ${gw_pid}). Stop with: ./run-gateway.sh --stop"
  exit 0
fi

echo "[3/3] Starting agy-acp-gateway in foreground (press Ctrl+C to stop) ..."
exec "${GW_BIN}" "${FINAL_ARGS[@]}"
