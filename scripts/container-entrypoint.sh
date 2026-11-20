#!/bin/bash
set -uo pipefail

: "${OMNI_API_KEYS:?Set OMNI_API_KEYS to comma-separated tenant:key entries}"
: "${OMNI_KEY_ADMIN_TOKEN:?Set a separate OMNI_KEY_ADMIN_TOKEN for key management}"

export OMNI_GATEWAY_CONFIG="${OMNI_GATEWAY_CONFIG:-/app/config/cloud-api.example.toml}"
export OMNI_USAGE_POLICY="${OMNI_USAGE_POLICY:-/app/policies/usage-tiers.toml}"
export OMNI_GATEWAY_STATE="${OMNI_GATEWAY_STATE:-/var/lib/omni/gateway-state.json}"
export OMNI_BIND_ADDR="${OMNI_BIND_ADDR:-127.0.0.1:8443}"
export OMNI_TLS_MODE=plain
export OMNI_GATEWAY_URL="${OMNI_GATEWAY_URL:-http://127.0.0.1:8443}"
export OMNI_GATEWAY_ALLOW_HTTP=1
export OMNI_GATEWAY_PROXY_LOCK=1
export OMNI_UI_BIND_ADDR=0.0.0.0
export PORT="${PORT:-3000}"
export OMNI_KEY_ADMIN_ENABLED=1

if [[ -n "${RENDER_EXTERNAL_HOSTNAME:-}" ]]; then
  export OMNI_PUBLIC_GATEWAY_URL="${OMNI_PUBLIC_GATEWAY_URL:-https://${RENDER_EXTERNAL_HOSTNAME}}"
  export OMNI_PUBLIC_SITE_URL="${OMNI_PUBLIC_SITE_URL:-https://${RENDER_EXTERNAL_HOSTNAME}}"
  export OMNI_ALLOWED_ORIGINS="${OMNI_ALLOWED_ORIGINS:-https://${RENDER_EXTERNAL_HOSTNAME}}"
  export OMNI_UI_ALLOWED_ORIGINS="${OMNI_UI_ALLOWED_ORIGINS:-https://${RENDER_EXTERNAL_HOSTNAME}}"
fi

start_gateway() {
  /usr/local/bin/omni-gateway &
  gateway_pid=$!
  gateway_started_at=$SECONDS
}

start_dashboard() {
  node /app/ui/server.js &
  dashboard_pid=$!
  dashboard_started_at=$SECONDS
}

gateway_backoff=2
dashboard_backoff=2
start_gateway
start_dashboard

cleanup() {
  trap - EXIT INT TERM
  for process_id in "$dashboard_pid" "$gateway_pid"; do
    if [[ -n "$process_id" ]] && kill -0 "$process_id" 2>/dev/null; then
      kill -TERM "$process_id" 2>/dev/null || true
    fi
  done
  for process_id in "$dashboard_pid" "$gateway_pid"; do
    if [[ -n "$process_id" ]]; then
      wait "$process_id" 2>/dev/null || true
    fi
  done
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

while true; do
  set +e
  wait -n "$gateway_pid" "$dashboard_pid"
  set -e

  if ! kill -0 "$gateway_pid" 2>/dev/null; then
    wait "$gateway_pid" 2>/dev/null || true
    uptime=$((SECONDS - gateway_started_at))
    if (( uptime >= 60 )); then gateway_backoff=2; fi
    delay=$gateway_backoff
    gateway_backoff=$((gateway_backoff < 30 ? gateway_backoff * 2 : 30))
    echo "Gateway exited; restarting it in ${delay}s." >&2
    sleep "$delay"
    start_gateway
  fi

  if ! kill -0 "$dashboard_pid" 2>/dev/null; then
    wait "$dashboard_pid" 2>/dev/null || true
    uptime=$((SECONDS - dashboard_started_at))
    if (( uptime >= 60 )); then dashboard_backoff=2; fi
    delay=$dashboard_backoff
    dashboard_backoff=$((dashboard_backoff < 30 ? dashboard_backoff * 2 : 30))
    echo "Dashboard bridge exited; restarting it in ${delay}s." >&2
    sleep "$delay"
    start_dashboard
  fi
done
