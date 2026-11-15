#!/usr/bin/env bash
set -euo pipefail

# One-command launcher for Liftgate + companion dashboard + live model discovery + Claude Code / Codex CLI.
#
# Usage:
#   ./start-agy.sh                    # Launch Claude Code (full bypass enabled by default)
#   ./start-agy.sh --codex            # Launch OpenAI Codex CLI (full bypass enabled by default)
#   ./start-agy.sh --no-bypass        # Launch without automatic permission/sandbox bypass
#   ./start-agy.sh --services-only    # Start gateway + dashboard and sync configs only
#   ./start-agy.sh --stop             # Stop background gateway and dashboard processes
#   ./start-agy.sh -p "Hello!"        # Forward arbitrary flags to Claude Code or Codex CLI

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUNTIME_DIR="${SCRIPT_DIR}/.agy-runtime"
mkdir -p "${RUNTIME_DIR}" "${SCRIPT_DIR}/.claude"

MODEL="${AGY_MODEL:-gemini-4-carbon-high}"
FAST_MODEL="${AGY_FAST_MODEL:-gemini-3.8-flash-low}"
EXPLICIT_FAST_MODEL=0
if [[ -n "${AGY_FAST_MODEL:-}" ]]; then
  EXPLICIT_FAST_MODEL=1
fi
GW_HOST="${AGY_HOST:-127.0.0.1}"
GW_PORT="${AGY_PORT:-8787}"
WEB_PORT="${AGY_WEB_PORT:-5173}"
API_KEY="${AGY_API_KEY:-local-agy}"
EXPLICIT_API_KEY=0
if [[ -n "${AGY_API_KEY:-}" ]]; then
  EXPLICIT_API_KEY=1
fi

SERVICES_ONLY=0
STOP_SERVICES=0
LAUNCH_CODEX=0
NO_BYPASS=0
SHOW_HELP=0
CLI_ARGS=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --)
      shift
      while [[ $# -gt 0 ]]; do
        CLI_ARGS+=("$1")
        shift
      done
      break
      ;;
    -h|--help|-Help)
      if [[ ${#CLI_ARGS[@]} -eq 0 ]]; then
        SHOW_HELP=1
      else
        CLI_ARGS+=("$1")
      fi
      shift
      ;;
    --gateway-model|-GatewayModel)
      shift
      if [[ $# -gt 0 ]]; then
        MODEL="$1"
        shift
      fi
      ;;
    --gateway-model=*)
      MODEL="${1#--gateway-model=}"
      shift
      ;;
    --fast-model|-FastModel)
      shift
      if [[ $# -gt 0 ]]; then
        FAST_MODEL="$1"
        EXPLICIT_FAST_MODEL=1
        shift
      fi
      ;;
    --fast-model=*)
      FAST_MODEL="${1#--fast-model=}"
      EXPLICIT_FAST_MODEL=1
      shift
      ;;
    --host|-ListenHost)
      shift
      if [[ $# -gt 0 ]]; then
        GW_HOST="$1"
        shift
      fi
      ;;
    --host=*)
      GW_HOST="${1#--host=}"
      shift
      ;;
    --port|-ListenPort)
      shift
      if [[ $# -gt 0 ]]; then
        GW_PORT="$1"
        shift
      fi
      ;;
    --port=*)
      GW_PORT="${1#--port=}"
      shift
      ;;
    --ui-port|--web-port|-UiPort)
      shift
      if [[ $# -gt 0 ]]; then
        WEB_PORT="$1"
        shift
      fi
      ;;
    --ui-port=*)
      WEB_PORT="${1#--ui-port=}"
      shift
      ;;
    --web-port=*)
      WEB_PORT="${1#--web-port=}"
      shift
      ;;
    --api-key|-ApiKey)
      shift
      if [[ $# -gt 0 ]]; then
        API_KEY="$1"
        EXPLICIT_API_KEY=1
        shift
      fi
      ;;
    --api-key=*)
      API_KEY="${1#--api-key=}"
      EXPLICIT_API_KEY=1
      shift
      ;;
    --services-only|--background-only|-BackgroundOnly)
      SERVICES_ONLY=1
      shift
      ;;
    --stop|--stop-services|-Stop|-StopServices)
      STOP_SERVICES=1
      shift
      ;;
    --codex|-Codex)
      LAUNCH_CODEX=1
      shift
      ;;
    --no-bypass|-NoBypass)
      NO_BYPASS=1
      shift
      ;;
    *)
      CLI_ARGS+=("$1")
      shift
      ;;
  esac
done

if [[ "${SHOW_HELP}" -eq 1 ]]; then
  echo "Usage: ./start-agy.sh [options] [--] [claude-or-codex-args...]"
  echo ""
  echo "Options:"
  echo "  --codex, -Codex                  Launch Codex CLI instead of Claude Code"
  echo "  --no-bypass, -NoBypass           Do not pass autonomous permission/sandbox bypass flags"
  echo "  --services-only, -BackgroundOnly Start gateway + dashboard and sync configs only"
  echo "  --stop, -StopServices            Stop background gateway and dashboard processes"
  echo "  --gateway-model <id>             Primary model (default: ${MODEL})"
  echo "  --fast-model <id>                Fast/Haiku model (default: ${FAST_MODEL})"
  echo "  --host <host>                    Gateway bind/connect host (default: ${GW_HOST})"
  echo "  --port <port>                    Gateway port (default: ${GW_PORT})"
  echo "  --ui-port <port>                 Companion Vite dev port (default: ${WEB_PORT})"
  echo "  --api-key <key>                  Bearer token / API key (default: local-agy)"
  echo "  -h, --help                       Show this help message"
  echo ""
  echo "By default, Claude Code starts with --dangerously-skip-permissions and Codex starts with"
  echo "--dangerously-bypass-approvals-and-sandbox. Remaining arguments pass through to the CLI."
  exit 0
fi

if [[ "${STOP_SERVICES}" -eq 1 ]]; then
  for entry in "Web Frontend:${RUNTIME_DIR}/web.pid" "liftgate:${RUNTIME_DIR}/gateway.pid"; do
    label="${entry%%:*}"
    pidfile="${entry#*:}"
    if [[ -f "${pidfile}" ]]; then
      pid="$(cat "${pidfile}" 2>/dev/null || true)"
      if [[ "${pid}" =~ ^[0-9]+$ ]] && kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        echo "[STOP] Stopped ${label} (PID ${pid})"
      fi
      rm -f "${pidfile}"
    fi
  done
  exit 0
fi

GW_URL="http://${GW_HOST}:${GW_PORT}"
WEB_URL="http://${GW_HOST}:${WEB_PORT}"
SETTINGS_PATH="${SCRIPT_DIR}/claude-carbon.settings.json"
LOCAL_SETTINGS_PATH="${SCRIPT_DIR}/.claude/settings.local.json"

DETECTED_PROXY="${HTTPS_PROXY:-${HTTP_PROXY:-}}"
export NO_PROXY="127.0.0.1,localhost,::1"
export no_proxy="127.0.0.1,localhost,::1"

# 1. Start liftgate if not running
if ! curl -fsS "${GW_URL}/healthz" >/dev/null 2>&1; then
  GW_BIN="${SCRIPT_DIR}/target/release/liftgate"
  if [[ ! -x "${GW_BIN}" && -x "${GW_BIN}.exe" ]]; then
    GW_BIN="${GW_BIN}.exe"
  fi
  if [[ ! -x "${GW_BIN}" ]]; then
    if [[ ! -f "${SCRIPT_DIR}/web/dist/index.html" ]] && command -v bun >/dev/null 2>&1; then
      echo "[1/4] Building embedded web bundle (web/dist) ..."
      bun install --cwd "${SCRIPT_DIR}/web" --frozen-lockfile && bun run --cwd "${SCRIPT_DIR}/web" build || true
    fi
    echo "[1/4] Building release liftgate ..."
    (cd "${SCRIPT_DIR}" && cargo build --release --locked -p liftgate)
    if [[ ! -x "${GW_BIN}" && -x "${GW_BIN}.exe" ]]; then
      GW_BIN="${GW_BIN}.exe"
    fi
  fi

  echo "[1/4] Starting liftgate on ${GW_URL} (default model: ${MODEL}) ..."
  GW_ARGS=("--listen" "${GW_HOST}:${GW_PORT}" "--default-model" "${MODEL}" "--acp-mode" "auto")
  if [[ "${EXPLICIT_API_KEY}" -eq 1 ]]; then
    GW_ARGS+=("--api-key" "${API_KEY}")
  fi
  nohup "${GW_BIN}" "${GW_ARGS[@]}" >"${RUNTIME_DIR}/gateway.out.log" 2>"${RUNTIME_DIR}/gateway.err.log" &
  gw_pid=$!
  echo "${gw_pid}" >"${RUNTIME_DIR}/gateway.pid"

  gw_ready=0
  for _ in {1..40}; do
    if ! kill -0 "${gw_pid}" 2>/dev/null; then
      echo "[ERROR] liftgate exited unexpectedly:" >&2
      tail -n 20 "${RUNTIME_DIR}/gateway.err.log" >&2 || true
      exit 1
    fi
    if curl -fsS "${GW_URL}/healthz" >/dev/null 2>&1; then
      gw_ready=1
      break
    fi
    sleep 0.3
  done
  if [[ "${gw_ready}" -ne 1 ]]; then
    echo "[ERROR] Timed out waiting for ${GW_URL}/healthz" >&2
    exit 1
  fi
  echo "      Gateway online (PID ${gw_pid}) -> ${GW_URL}/"
else
  echo "[1/4] Gateway already running at ${GW_URL}"
fi

# 2. Start Bun + Vite companion frontend if not running
if ! curl -fsS "${WEB_URL}/" >/dev/null 2>&1; then
  if command -v bun >/dev/null 2>&1; then
    if [[ ! -d "${SCRIPT_DIR}/web/node_modules" ]]; then
      echo "[2/4] Installing frontend dependencies via Bun ..."
      bun install --cwd "${SCRIPT_DIR}/web"
    fi
    echo "[2/4] Starting companion frontend on ${WEB_URL}/ ..."
    VITE_GATEWAY_PROXY="${GW_URL}" nohup bun run --cwd "${SCRIPT_DIR}/web" dev --host "${GW_HOST}" --port "${WEB_PORT}" \
      >"${RUNTIME_DIR}/web.out.log" 2>"${RUNTIME_DIR}/web.err.log" &
    web_pid=$!
    echo "${web_pid}" >"${RUNTIME_DIR}/web.pid"
    web_ready=0
    for _ in {1..30}; do
      if ! kill -0 "${web_pid}" 2>/dev/null; then
        break
      fi
      if curl -fsS "${WEB_URL}/" >/dev/null 2>&1; then
        web_ready=1
        break
      fi
      sleep 0.3
    done
    if [[ "${web_ready}" -eq 1 ]]; then
      echo "      Frontend online (PID ${web_pid}) -> ${WEB_URL}/"
    else
      echo "      Frontend dev server unavailable or warming up; embedded UI ready at ${GW_URL}/"
    fi
  else
    echo "[2/4] bun not found in PATH; embedded UI is live at ${GW_URL}/"
  fi
else
  echo "[2/4] Companion frontend already running at ${WEB_URL}/"
fi

# 3. Dynamic model discovery (/v1/models) -> Claude Code & Codex CLI
echo "[3/4] Discovering live models from ${GW_URL}/v1/models ..."
MODELS_JSON_PATH="${RUNTIME_DIR}/models.json"
curl -fsS -H "Authorization: Bearer ${API_KEY}" "${GW_URL}/v1/models" -o "${MODELS_JSON_PATH}"

sync_claude_settings() {
  local js_script='
    const fs = require("fs");
    const body = JSON.parse(fs.readFileSync(process.env.MODELS_JSON_PATH, "utf8"));
    const data = Array.isArray(body.data) ? body.data : [];
    if (process.env.CHECK_COUNT_ONLY === "1") {
      console.log(String(data.length));
      process.exit(0);
    }
    let fastModel = process.env.FAST_MODEL;
    if (process.env.EXPLICIT_FAST_MODEL !== "1") {
      const flash = data
        .map((m) => String(m.id))
        .filter((id) => /^gemini-.*flash.*-low$/.test(id))
        .sort()
        .reverse();
      if (flash.length > 0) {
        fastModel = flash[0];
      }
    }
    const modelSettings = {};
    const compactLimits = new Map();
    const addCompactWindow = (id, limit) => {
      const previous = modelSettings[id]?.autoCompactWindow;
      modelSettings[id] = { autoCompactWindow: previous ? Math.min(previous, limit) : limit };
    };
    const options = data.map((m) => {
      const id = String(m.id);
      const lower = id.toLowerCase();
      const behavesAs = lower.startsWith("gpt-oss")
        ? "claude-haiku-4-5-20251001"
        : lower.startsWith("gemini-")
          ? (lower.includes("flash-low") ? "claude-haiku-5-5"
            : (lower.includes("sonnet") || lower.endsWith("-medium")) ? "claude-sonnet-5-5" : "claude-opus-5-5")
        : (lower.includes("sonnet") || lower.endsWith("-medium"))
          ? "claude-sonnet-4-6"
          : "claude-opus-4-6";
      const ctx = m.context_window && Number(m.context_window) > 0
        ? `${Math.round(Number(m.context_window) / 1000)}k ctx`
        : "unknown ctx";
      // An older gateway may advertise an inflated window without a safe limit.
      const limit = Number.isSafeInteger(m.auto_compact_token_limit) && m.auto_compact_token_limit >= 100000
        ? Math.min(m.auto_compact_token_limit, 1000000) : 110000;
      compactLimits.set(id, limit);
      addCompactWindow(id, limit);
      addCompactWindow(behavesAs, limit);
      return {
        model: id,
        label: m.display_name || id,
        description: `${ctx} \u00b7 Antigravity ACP (${id})`,
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
      ANTHROPIC_DEFAULT_HAIKU_MODEL: fastModel,
      ANTHROPIC_SMALL_FAST_MODEL: fastModel,
      CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC: "1",
      CLAUDE_CODE_AUTO_MODE_SERVER: "0",
      NO_PROXY: "127.0.0.1,localhost,::1",
    };
    const proxyUrl = process.env.DETECTED_PROXY || "";
    if (proxyUrl) {
      envObj.HTTP_PROXY = proxyUrl;
      envObj.HTTPS_PROXY = proxyUrl;
    }
    const cfg = { model: process.env.MODEL };
    if (process.env.NO_BYPASS !== "1") {
      cfg.skipDangerousModePermissionPrompt = true;
      cfg.permissions = { defaultMode: "bypassPermissions" };
    }
    cfg.env = envObj;
    cfg.modelOverrides = {
      "claude-fable-5-2": process.env.MODEL,
      "claude-opus-5-5": process.env.MODEL,
      "claude-sonnet-5-5": process.env.MODEL,
      "claude-haiku-5-5": fastModel,
      "claude-opus-4-6": process.env.MODEL,
      "claude-sonnet-4-6": process.env.MODEL,
      "claude-haiku-4-5-20251001": fastModel,
    };
    for (const [alias, target] of Object.entries(cfg.modelOverrides)) {
      addCompactWindow(alias, compactLimits.get(target) || 110000);
    }
    cfg.autoCompactWindow = 110000;
    cfg.modelSettings = modelSettings;
    cfg.modelPicker = { replaceBuiltInOptions: true, options };
    const json = JSON.stringify(cfg, null, 2);
    fs.writeFileSync(process.env.SETTINGS_PATH, json, "utf8");
    fs.writeFileSync(process.env.LOCAL_SETTINGS_PATH, json, "utf8");
    console.log(`${options.length}\t${fastModel}`);
  '

  local py_script='
import json, os, re

with open(os.environ["MODELS_JSON_PATH"], "r", encoding="utf-8") as f:
    body = json.load(f)
data = body.get("data") or []
if os.environ.get("CHECK_COUNT_ONLY") == "1":
    print(len(data))
    raise SystemExit(0)

fast_model = os.environ["FAST_MODEL"]
if os.environ.get("EXPLICIT_FAST_MODEL") != "1":
    flash = sorted(
        (str(m.get("id", "")) for m in data if re.match(r"^gemini-.*flash.*-low$", str(m.get("id", "")))),
        reverse=True,
    )
    if flash:
        fast_model = flash[0]

options = []
model_settings = {}
compact_limits = {}
def add_compact_window(mid, limit):
    previous = model_settings.get(mid, {}).get("autoCompactWindow", limit)
    model_settings[mid] = {"autoCompactWindow": min(previous, limit)}

for m in data:
    mid = str(m.get("id", ""))
    lower = mid.lower()
    if lower.startswith("gpt-oss"):
        behaves_as = "claude-haiku-4-5-20251001"
    elif lower.startswith("gemini-"):
        if "flash-low" in lower:
            behaves_as = "claude-haiku-5-5"
        elif "sonnet" in lower or lower.endswith("-medium"):
            behaves_as = "claude-sonnet-5-5"
        else:
            behaves_as = "claude-opus-5-5"
    elif "sonnet" in lower or lower.endswith("-medium"):
        behaves_as = "claude-sonnet-4-6"
    else:
        behaves_as = "claude-opus-4-6"
    cw = m.get("context_window")
    ctx = f"{round(int(cw) / 1000)}k ctx" if cw and int(cw) > 0 else "unknown ctx"
    limit = m.get("auto_compact_token_limit")
    limit = min(limit, 1000000) if isinstance(limit, int) and limit >= 100000 else 110000
    compact_limits[mid] = limit
    add_compact_window(mid, limit)
    add_compact_window(behaves_as, limit)
    options.append({
        "model": mid,
        "label": m.get("display_name") or mid,
        "description": f"{ctx} \u00b7 Antigravity ACP ({mid})",
        "behavesAs": behaves_as,
    })

env_obj = {
    "ANTHROPIC_BASE_URL": os.environ["GW_URL"],
    "ANTHROPIC_AUTH_TOKEN": os.environ["API_KEY"],
    "ANTHROPIC_API_KEY": "",
    "ANTHROPIC_MODEL": os.environ["MODEL"],
    "ANTHROPIC_DEFAULT_OPUS_MODEL": os.environ["MODEL"],
    "ANTHROPIC_DEFAULT_SONNET_MODEL": os.environ["MODEL"],
    "ANTHROPIC_DEFAULT_HAIKU_MODEL": fast_model,
    "ANTHROPIC_SMALL_FAST_MODEL": fast_model,
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
    "CLAUDE_CODE_AUTO_MODE_SERVER": "0",
    "NO_PROXY": "127.0.0.1,localhost,::1",
}
proxy_url = os.environ.get("DETECTED_PROXY") or ""
if proxy_url:
    env_obj["HTTP_PROXY"] = proxy_url
    env_obj["HTTPS_PROXY"] = proxy_url

cfg = {"model": os.environ["MODEL"]}
if os.environ.get("NO_BYPASS") != "1":
    cfg["skipDangerousModePermissionPrompt"] = True
    cfg["permissions"] = {"defaultMode": "bypassPermissions"}
cfg["env"] = env_obj
cfg["modelOverrides"] = {
    "claude-fable-5-2": os.environ["MODEL"],
    "claude-opus-5-5": os.environ["MODEL"],
    "claude-sonnet-5-5": os.environ["MODEL"],
    "claude-haiku-5-5": fast_model,
    "claude-opus-4-6": os.environ["MODEL"],
    "claude-sonnet-4-6": os.environ["MODEL"],
    "claude-haiku-4-5-20251001": fast_model,
}
for alias, target in cfg["modelOverrides"].items():
    add_compact_window(alias, compact_limits.get(target, 110000))
cfg["autoCompactWindow"] = 110000
cfg["modelSettings"] = model_settings
cfg["modelPicker"] = {"replaceBuiltInOptions": True, "options": options}
payload = json.dumps(cfg, ensure_ascii=False, indent=2)
for p in (os.environ["SETTINGS_PATH"], os.environ["LOCAL_SETTINGS_PATH"]):
    with open(p, "w", encoding="utf-8") as out:
        out.write(payload)
print(f"{len(options)}\t{fast_model}")
'

  if command -v bun >/dev/null 2>&1; then
    bun -e "${js_script}"
  elif command -v node >/dev/null 2>&1; then
    node -e "${js_script}"
  elif command -v python3 >/dev/null 2>&1; then
    python3 -c "${py_script}"
  else
    return 1
  fi
}

MODEL_COUNT="$(
  MODELS_JSON_PATH="${MODELS_JSON_PATH}" CHECK_COUNT_ONLY="1" \
  GW_URL="${GW_URL}" API_KEY="${API_KEY}" MODEL="${MODEL}" FAST_MODEL="${FAST_MODEL}" \
  EXPLICIT_FAST_MODEL="${EXPLICIT_FAST_MODEL}" DETECTED_PROXY="${DETECTED_PROXY}" \
  NO_BYPASS="${NO_BYPASS}" SETTINGS_PATH="${SETTINGS_PATH}" LOCAL_SETTINGS_PATH="${LOCAL_SETTINGS_PATH}" \
  sync_claude_settings 2>/dev/null || echo "0"
)"

if [[ "${MODEL_COUNT}" =~ ^[0-9]+$ && "${MODEL_COUNT}" -le 1 ]] && command -v agy >/dev/null 2>&1; then
  echo "      Antigravity CLI requires Google sign-in; launching 'agy acp --login' ..."
  (unset AGY_API_KEY; agy acp --login)
  curl -fsS -H "Authorization: Bearer ${API_KEY}" "${GW_URL}/v1/models" -o "${MODELS_JSON_PATH}"
fi

SYNC_RESULT="$(
  MODELS_JSON_PATH="${MODELS_JSON_PATH}" CHECK_COUNT_ONLY="0" \
  GW_URL="${GW_URL}" API_KEY="${API_KEY}" MODEL="${MODEL}" FAST_MODEL="${FAST_MODEL}" \
  EXPLICIT_FAST_MODEL="${EXPLICIT_FAST_MODEL}" DETECTED_PROXY="${DETECTED_PROXY}" \
  NO_BYPASS="${NO_BYPASS}" SETTINGS_PATH="${SETTINGS_PATH}" LOCAL_SETTINGS_PATH="${LOCAL_SETTINGS_PATH}" \
  sync_claude_settings || true
)"

SYNCED_COUNT="0"
if [[ -n "${SYNC_RESULT}" ]]; then
  SYNCED_COUNT="${SYNC_RESULT%%$'\t'*}"
  RESOLVED_FAST="${SYNC_RESULT#*$'\t'}"
  if [[ -n "${RESOLVED_FAST}" && "${RESOLVED_FAST}" != "${SYNC_RESULT}" ]]; then
    FAST_MODEL="${RESOLVED_FAST}"
  fi
fi

CODEX_CATALOG_PATH="${RUNTIME_DIR}/codex-model-catalog.json"
curl -fsS -G --data-urlencode "default_model=${MODEL}" -H "Authorization: Bearer ${API_KEY}" \
  "${GW_URL}/v1/codex-models" -o "${CODEX_CATALOG_PATH}"

CODEX_BYPASS_LINES=""
if [[ "${NO_BYPASS}" -eq 0 ]]; then
  CODEX_BYPASS_LINES=$'approval_policy = "never"\nsandbox_mode = "danger-full-access"\n'
fi

cat >"${RUNTIME_DIR}/codex-config.toml" <<EOF
model = "${MODEL}"
model_provider = "agy"
model_catalog_json = "${CODEX_CATALOG_PATH}"
${CODEX_BYPASS_LINES}web_search = "live"

[model_providers.agy]
name = "Liftgate"
base_url = "${GW_URL}/v1"
env_key = "AGY_GATEWAY_API_KEY"
wire_api = "responses"
EOF

echo "      Synced ${SYNCED_COUNT} live models into Claude modelPicker & Codex model_catalog_json (.agy-runtime/codex-model-catalog.json)"

# 4. Configure scoped process environment & write .agy-runtime/env.sh
# OPENAI_BASE_URL and OPENAI_API_KEY are intentionally NOT set globally
# so plain `codex` outside `--codex` preserves your default Codex setup.
unset ANTHROPIC_API_KEY || true
unset AGY_API_KEY || true
export ANTHROPIC_BASE_URL="${GW_URL}"
export ANTHROPIC_AUTH_TOKEN="${API_KEY}"
export ANTHROPIC_MODEL="${MODEL}"
export ANTHROPIC_DEFAULT_OPUS_MODEL="${MODEL}"
export ANTHROPIC_DEFAULT_SONNET_MODEL="${MODEL}"
export ANTHROPIC_DEFAULT_HAIKU_MODEL="${FAST_MODEL}"
export ANTHROPIC_SMALL_FAST_MODEL="${FAST_MODEL}"
unset CLAUDE_CODE_MAX_CONTEXT_TOKENS CLAUDE_CODE_AUTO_COMPACT_WINDOW
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1"
export CLAUDE_CODE_AUTO_MODE_SERVER="0"
export AGY_GATEWAY_API_KEY="${API_KEY}"
export NO_PROXY="127.0.0.1,localhost,::1"
export no_proxy="127.0.0.1,localhost,::1"
if [[ -n "${DETECTED_PROXY}" ]]; then
  export HTTP_PROXY="${DETECTED_PROXY}"
  export HTTPS_PROXY="${DETECTED_PROXY}"
fi

cat >"${RUNTIME_DIR}/env.sh" <<EOF
unset ANTHROPIC_API_KEY
unset AGY_API_KEY
export ANTHROPIC_BASE_URL="${GW_URL}"
export ANTHROPIC_AUTH_TOKEN="${API_KEY}"
export ANTHROPIC_MODEL="${MODEL}"
export ANTHROPIC_DEFAULT_OPUS_MODEL="${MODEL}"
export ANTHROPIC_DEFAULT_SONNET_MODEL="${MODEL}"
export ANTHROPIC_DEFAULT_HAIKU_MODEL="${FAST_MODEL}"
export ANTHROPIC_SMALL_FAST_MODEL="${FAST_MODEL}"
unset CLAUDE_CODE_MAX_CONTEXT_TOKENS CLAUDE_CODE_AUTO_COMPACT_WINDOW
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1"
export CLAUDE_CODE_AUTO_MODE_SERVER="0"
export AGY_GATEWAY_API_KEY="${API_KEY}"
export NO_PROXY="127.0.0.1,localhost,::1"
EOF

echo "[4/4] Scoped environment ready (caller shell environment remains untouched)."

BYPASS_LABEL="full autonomous bypass"
if [[ "${NO_BYPASS}" -eq 1 ]]; then
  BYPASS_LABEL="standard permissions (--no-bypass)"
fi

echo ""
echo "  Gateway API & Embedded UI : ${GW_URL}/"
echo "  Companion Dev Frontend    : ${WEB_URL}/"
echo "  Models Discovery Endpoint : ${GW_URL}/v1/models (${SYNCED_COUNT} models)"
echo "  Active Primary Model      : ${MODEL} (Fast/Subagent: ${FAST_MODEL})"
echo "  Execution Mode            : ${BYPASS_LABEL}"
echo ""

if [[ "${SERVICES_ONLY}" -eq 1 ]]; then
  exit 0
fi

if [[ "${LAUNCH_CODEX}" -eq 1 ]]; then
  HAS_EXPLICIT_MODEL=0
  HAS_EXPLICIT_APPROVAL=0
  if [[ ${#CLI_ARGS[@]} -gt 0 ]]; then
    for arg in "${CLI_ARGS[@]}"; do
      case "${arg}" in
        -m|--model|-m=*|--model=*)
          HAS_EXPLICIT_MODEL=1
          ;;
        --dangerously-bypass-approvals-and-sandbox|--yolo|-s|--sandbox|-s=*|--sandbox=*|-a|--ask-for-approval|-a=*|--ask-for-approval=*|--approve-for-me|--full-auto)
          HAS_EXPLICIT_APPROVAL=1
          ;;
      esac
    done
  fi

  CODEX_LAUNCH_ARGS=()
  if [[ "${NO_BYPASS}" -eq 0 && "${HAS_EXPLICIT_APPROVAL}" -eq 0 ]]; then
    CODEX_LAUNCH_ARGS+=("--dangerously-bypass-approvals-and-sandbox")
  fi
  CODEX_LAUNCH_ARGS+=("-c" "model_provider='agy'")
  if [[ "${HAS_EXPLICIT_MODEL}" -eq 0 ]]; then
    CODEX_LAUNCH_ARGS+=("-c" "model='${MODEL}'")
  fi
  CODEX_LAUNCH_ARGS+=(
    "-c" "model_catalog_json='${CODEX_CATALOG_PATH}'"
    "-c" "web_search='live'"
    "-c" "model_providers.agy.name='Liftgate'"
    "-c" "model_providers.agy.base_url='${GW_URL}/v1'"
    "-c" "model_providers.agy.env_key='AGY_GATEWAY_API_KEY'"
    "-c" "model_providers.agy.wire_api='responses'"
  )
  if [[ ${#CLI_ARGS[@]} -gt 0 ]]; then
    CODEX_LAUNCH_ARGS+=("${CLI_ARGS[@]}")
  fi
  exec codex "${CODEX_LAUNCH_ARGS[@]}"
fi

HAS_EXPLICIT_CLAUDE_MODEL=0
HAS_EXPLICIT_CLAUDE_PERM=0
if [[ ${#CLI_ARGS[@]} -gt 0 ]]; then
  for arg in "${CLI_ARGS[@]}"; do
    case "${arg}" in
      --model|--model=*)
        HAS_EXPLICIT_CLAUDE_MODEL=1
        ;;
      --dangerously-skip-permissions|--allow-dangerously-skip-permissions|--permission-mode|--permission-mode=*|--safe-mode|--restricted)
        HAS_EXPLICIT_CLAUDE_PERM=1
        ;;
    esac
  done
fi

CLAUDE_LAUNCH_ARGS=()
if [[ "${NO_BYPASS}" -eq 0 && "${HAS_EXPLICIT_CLAUDE_PERM}" -eq 0 ]]; then
  CLAUDE_LAUNCH_ARGS+=("--dangerously-skip-permissions")
fi
CLAUDE_LAUNCH_ARGS+=("--setting-sources" "project,local" "--settings" "${SETTINGS_PATH}")
if [[ "${HAS_EXPLICIT_CLAUDE_MODEL}" -eq 0 ]]; then
  CLAUDE_LAUNCH_ARGS+=("--model" "${MODEL}")
fi
if [[ ${#CLI_ARGS[@]} -gt 0 ]]; then
  CLAUDE_LAUNCH_ARGS+=("${CLI_ARGS[@]}")
fi
exec claude "${CLAUDE_LAUNCH_ARGS[@]}"
