#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNTIME_DIR="${SCRIPT_DIR}/.agy-runtime"
mkdir -p "${RUNTIME_DIR}" "${SCRIPT_DIR}/.claude"

MODEL="${AGY_MODEL:-gemini-4-carbon-high}"
FAST_MODEL="${AGY_FAST_MODEL:-gemini-3.8-flash-low}"
GW_HOST="${AGY_HOST:-127.0.0.1}"
GW_PORT="${AGY_PORT:-8787}"
WEB_PORT="${AGY_WEB_PORT:-5173}"
API_KEY="${AGY_API_KEY:-local-agy}"

GW_URL="http://${GW_HOST}:${GW_PORT}"
WEB_URL="http://${GW_HOST}:${WEB_PORT}"
SETTINGS_PATH="${SCRIPT_DIR}/claude-carbon.settings.json"
LOCAL_SETTINGS_PATH="${SCRIPT_DIR}/.claude/settings.local.json"

if [[ "${1:-}" == "--stop" ]]; then
  for pidfile in "${RUNTIME_DIR}/web.pid" "${RUNTIME_DIR}/gateway.pid"; do
    if [[ -f "${pidfile}" ]]; then
      pid="$(cat "${pidfile}")"
      kill "${pid}" 2>/dev/null || true
      rm -f "${pidfile}"
    fi
  done
  echo "[STOP] Stopped background services."
  exit 0
fi

SERVICES_ONLY=0
LAUNCH_CODEX=0
if [[ "${1:-}" == "--services-only" ]]; then
  SERVICES_ONLY=1
  shift
elif [[ "${1:-}" == "--codex" ]]; then
  LAUNCH_CODEX=1
  shift
fi

# 1. Start liftgate if not running
if ! curl -fsS "${GW_URL}/healthz" >/dev/null 2>&1; then
  GW_BIN="${SCRIPT_DIR}/target/release/liftgate"
  if [[ ! -x "${GW_BIN}" ]]; then
    echo "[1/4] Building release liftgate ..."
    (cd "${SCRIPT_DIR}" && cargo build --release -p liftgate)
  fi
  echo "[1/4] Starting liftgate on ${GW_URL} ..."
  nohup "${GW_BIN}" --listen "${GW_HOST}:${GW_PORT}" --default-model "${MODEL}" --acp-mode auto \
    >"${RUNTIME_DIR}/gateway.out.log" 2>"${RUNTIME_DIR}/gateway.err.log" &
  echo $! >"${RUNTIME_DIR}/gateway.pid"
  for _ in {1..40}; do
    if curl -fsS "${GW_URL}/healthz" >/dev/null 2>&1; then break; fi
    sleep 0.3
  done
else
  echo "[1/4] Gateway already running at ${GW_URL}"
fi

# 2. Start Bun + Vite companion frontend if not running
if ! curl -fsS "${WEB_URL}/" >/dev/null 2>&1; then
  if command -v bun >/dev/null 2>&1; then
    if [[ ! -d "${SCRIPT_DIR}/web/node_modules" ]]; then
      bun install --cwd "${SCRIPT_DIR}/web"
    fi
    echo "[2/4] Starting companion frontend on ${WEB_URL} ..."
    VITE_GATEWAY_PROXY="${GW_URL}" nohup bun run --cwd "${SCRIPT_DIR}/web" dev --host "${GW_HOST}" --port "${WEB_PORT}" \
      >"${RUNTIME_DIR}/web.out.log" 2>"${RUNTIME_DIR}/web.err.log" &
    echo $! >"${RUNTIME_DIR}/web.pid"
    for _ in {1..25}; do
      if curl -fsS "${WEB_URL}/" >/dev/null 2>&1; then break; fi
      sleep 0.3
    done
  else
    echo "[2/4] bun not found in PATH; embedded UI available at ${GW_URL}/"
  fi
else
  echo "[2/4] Companion frontend already running at ${WEB_URL}/"
fi

# 3. Dynamic model discovery via Bun/Node JSON builder
echo "[3/4] Syncing live model discovery from ${GW_URL}/v1/models ..."
if command -v bun >/dev/null 2>&1; then
  GW_URL="${GW_URL}" API_KEY="${API_KEY}" MODEL="${MODEL}" FAST_MODEL="${FAST_MODEL}" \
  SETTINGS_PATH="${SETTINGS_PATH}" LOCAL_SETTINGS_PATH="${LOCAL_SETTINGS_PATH}" \
  bun -e '
    const res = await fetch(`${process.env.GW_URL}/v1/models`, {
      headers: { Authorization: `Bearer ${process.env.API_KEY}` }
    });
    const body = await res.json();
    const options = (body.data ?? []).map((m) => {
      const id = String(m.id);
      const lower = id.toLowerCase();
      const behavesAs = lower.includes("flash-low")
        ? "claude-haiku-4-5-20251001"
        : (lower.includes("sonnet") || lower.endsWith("-medium"))
          ? "claude-sonnet-4-6"
          : "claude-opus-4-6";
      const ctx = m.context_window ? `${Math.round(m.context_window / 1000)}k ctx` : "1000k ctx";
      return {
        model: id,
        label: m.display_name || id,
        description: `${ctx} · Antigravity ACP (${id})`,
        behavesAs,
      };
    });
    const envObj = {
      ANTHROPIC_BASE_URL: process.env.GW_URL,
      ANTHROPIC_AUTH_TOKEN: process.env.API_KEY,
      ANTHROPIC_API_KEY: "",
      ANTHROPIC_MODEL: process.env.MODEL,
      ANTHROPIC_DEFAULT_OPUS_MODEL: process.env.MODEL,
      ANTHROPIC_DEFAULT_SONNET_MODEL: process.env.MODEL,
      ANTHROPIC_DEFAULT_HAIKU_MODEL: process.env.FAST_MODEL,
      ANTHROPIC_SMALL_FAST_MODEL: process.env.FAST_MODEL,
      CLAUDE_CODE_MAX_CONTEXT_TOKENS: "1000000",
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
      CLAUDE_CODE_AUTO_MODE_SERVER: "0",
      NO_PROXY: "127.0.0.1,localhost,::1",
    };
    const proxyUrl = process.env.HTTPS_PROXY || process.env.HTTP_PROXY || "";
    if (proxyUrl) {
      envObj.HTTP_PROXY = proxyUrl;
      envObj.HTTPS_PROXY = proxyUrl;
    }
    const cfg = {
      model: process.env.MODEL,
      env: envObj,
      modelOverrides: {
        "claude-opus-4-6": process.env.MODEL,
        "claude-sonnet-4-6": process.env.MODEL,
        "claude-haiku-4-5-20251001": process.env.FAST_MODEL,
      },
      modelPicker: { replaceBuiltInOptions: true, options },
    };
    const json = JSON.stringify(cfg, null, 2);
    await Bun.write(process.env.SETTINGS_PATH, json);
    await Bun.write(process.env.LOCAL_SETTINGS_PATH, json);
    console.log(`      Synced ${options.length} live models into Claude modelPicker.`);
  '
fi

CODEX_CATALOG_PATH="${RUNTIME_DIR}/codex-model-catalog.json"
curl -fsS -H "Authorization: Bearer ${API_KEY}" "${GW_URL}/v1/codex-models?default_model=${MODEL}" -o "${CODEX_CATALOG_PATH}"
cat >"${RUNTIME_DIR}/codex-config.toml" <<EOF
model = "${MODEL}"
model_provider = "agy"
model_catalog_json = "${CODEX_CATALOG_PATH}"
web_search = "live"
model_context_window = 1000000
model_auto_compact_token_limit = 850000

[model_providers.agy]
name = "Liftgate"
base_url = "${GW_URL}/v1"
env_key = "AGY_GATEWAY_API_KEY"
wire_api = "responses"
EOF
echo "      Synced live Antigravity model catalog for Codex (${CODEX_CATALOG_PATH})."

# 4. Export environment variables
unset ANTHROPIC_API_KEY || true
unset AGY_API_KEY || true
export NO_PROXY="127.0.0.1,localhost,::1"
export no_proxy="127.0.0.1,localhost,::1"
export ANTHROPIC_BASE_URL="${GW_URL}"
export ANTHROPIC_AUTH_TOKEN="${API_KEY}"
export ANTHROPIC_MODEL="${MODEL}"
export ANTHROPIC_DEFAULT_OPUS_MODEL="${MODEL}"
export ANTHROPIC_DEFAULT_SONNET_MODEL="${MODEL}"
export ANTHROPIC_DEFAULT_HAIKU_MODEL="${FAST_MODEL}"
export ANTHROPIC_SMALL_FAST_MODEL="${FAST_MODEL}"
export CLAUDE_CODE_MAX_CONTEXT_TOKENS="1000000"
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1"
export CLAUDE_CODE_AUTO_MODE_SERVER="0"
export AGY_GATEWAY_API_KEY="${API_KEY}"

echo ""
echo "  Gateway API & Embedded UI : ${GW_URL}/"
echo "  Companion Dev Frontend    : ${WEB_URL}/"
echo "  Active Primary Model      : ${MODEL}"
echo ""

if [[ "${SERVICES_ONLY}" -eq 1 ]]; then
  exit 0
fi

if [[ "${LAUNCH_CODEX}" -eq 1 ]]; then
  HAS_EXPLICIT_MODEL=0
  for arg in "$@"; do
    case "${arg}" in
      -m|--model|-m=*|--model=*) HAS_EXPLICIT_MODEL=1; break ;;
    esac
  done
  if [[ "${HAS_EXPLICIT_MODEL}" -eq 1 ]]; then
    exec codex \
      -c 'model_provider="agy"' \
      -c "model_catalog_json=\"${CODEX_CATALOG_PATH}\"" \
      -c 'web_search="live"' \
      -c 'model_context_window=1000000' \
      -c 'model_auto_compact_token_limit=850000' \
      -c "model_providers.agy={name=\"Liftgate\",base_url=\"${GW_URL}/v1\",env_key=\"AGY_GATEWAY_API_KEY\",wire_api=\"responses\"}" \
      "$@"
  else
    exec codex \
      -c 'model_provider="agy"' \
      -c "model=\"${MODEL}\"" \
      -c "model_catalog_json=\"${CODEX_CATALOG_PATH}\"" \
      -c 'web_search="live"' \
      -c 'model_context_window=1000000' \
      -c 'model_auto_compact_token_limit=850000' \
      -c "model_providers.agy={name=\"Liftgate\",base_url=\"${GW_URL}/v1\",env_key=\"AGY_GATEWAY_API_KEY\",wire_api=\"responses\"}" \
      "$@"
  fi
fi

exec claude --setting-sources "project,local" --settings "${SETTINGS_PATH}" --model "${MODEL}" "$@"
