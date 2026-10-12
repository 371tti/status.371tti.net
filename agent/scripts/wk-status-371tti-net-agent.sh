#!/usr/bin/env bash
set -Eeuo pipefail

SERVICE_NAME="wk-status-371tti-net-agent"
SERVICE_USER="wkstatus-agent"
BINARY_NAME="wk-status-371tti-net-agent"
RELEASE_BINARY_URL="https://github.com/371tti/status.371tti.net/releases/latest/download/${BINARY_NAME}"
CONFIG_DIR="/etc/${SERVICE_NAME}"
CONFIG_FILE="${CONFIG_DIR}/agent.env"
MONITOR_META_FILE="${CONFIG_DIR}/monitor.env"
UNIT_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
INSTALL_PATH="/usr/local/bin/${BINARY_NAME}"
SCRIPT_PATH="$(readlink -f "${BASH_SOURCE[0]}")"
SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_PATH")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
ORIGINAL_ARGS=("$@")
TEMP_BINARY=""
SOURCE_BINARY=""
MONITOR_ID=""
MONITOR_NAME=""
MONITOR_HOST=""
MONITOR_INTERVAL="60"
MONITOR_TIMEOUT="5"
MONITOR_SLOW_MS="500"
HOST_CANDIDATE_ADDRESSES=()
HOST_CANDIDATE_LABELS=()
CONFIGURE_FROM_INSTALL="0"

cleanup() {
  if [[ -n "$TEMP_BINARY" ]]; then
    rm -f -- "$TEMP_BINARY"
  fi
}
trap cleanup EXIT

fail() {
  printf 'Error: %s\n' "$*" >&2
  exit 1
}

usage() {
  cat <<EOF
Usage:
  bash $0 install
  bash $0 configure
  bash $0 uninstall [--purge]

Commands:
  install      Install the bundled or latest Release binary and start the service.
  configure    Ask for monitor identity/network settings and generate server YAML.
  uninstall    Remove the service and binary; keep configuration by default.
  --purge      Also remove the configuration, generated token, and monitor details.
EOF
}

require_linux_systemd() {
  [[ "$(uname -s)" == "Linux" ]] || fail "This installer supports Linux only."
  command -v systemctl >/dev/null 2>&1 || fail "systemd is required."
}

require_root() {
  if [[ "$EUID" -ne 0 ]]; then
    command -v sudo >/dev/null 2>&1 || fail "Run as root or install sudo."
    exec sudo -- bash "$SCRIPT_PATH" "${ORIGINAL_ARGS[@]}"
  fi
}

load_config() {
  LISTEN_ADDRESS="0.0.0.0"
  LISTEN_PORT="9791"
  ACCESS_TOKEN=""
  ENABLE_CPU="1"
  ENABLE_MEMORY="1"
  ENABLE_LOAD="1"
  ENABLE_NETWORK="1"
  WARN_CPU_PERCENT="90"
  WARN_MEMORY_PERCENT="90"
  WARN_IOWAIT_PERCENT="20"
  WARN_NIC_ERROR_DROP_PERCENT="1"
  STATUS_NETWORK_MIN_PACKETS="100"
  if [[ -r "$CONFIG_FILE" ]]; then
    while IFS='=' read -r key value; do
      case "$key" in
        WS371_AGENT_BIND) LISTEN_ADDRESS="$value" ;;
        WS371_AGENT_PORT) LISTEN_PORT="$value" ;;
        WS371_AGENT_TOKEN) ACCESS_TOKEN="$value" ;;
        WS371_AGENT_ENABLE_CPU) ENABLE_CPU="$value" ;;
        WS371_AGENT_ENABLE_MEMORY) ENABLE_MEMORY="$value" ;;
        WS371_AGENT_ENABLE_LOAD) ENABLE_LOAD="$value" ;;
        WS371_AGENT_ENABLE_NETWORK) ENABLE_NETWORK="$value" ;;
        WS371_AGENT_WARN_CPU_PERCENT) WARN_CPU_PERCENT="$value" ;;
        WS371_AGENT_WARN_MEMORY_PERCENT) WARN_MEMORY_PERCENT="$value" ;;
        WS371_AGENT_WARN_IOWAIT_PERCENT) WARN_IOWAIT_PERCENT="$value" ;;
        WS371_AGENT_WARN_NIC_ERROR_DROP_PERCENT) WARN_NIC_ERROR_DROP_PERCENT="$value" ;;
        WS371_AGENT_STATUS_NETWORK_MIN_PACKETS) STATUS_NETWORK_MIN_PACKETS="$value" ;;
      esac
    done < "$CONFIG_FILE"
  fi
}

load_monitor_metadata() {
  local host_slug
  host_slug="$(hostname -s 2>/dev/null || printf 'agent')"
  host_slug="$(printf '%s' "$host_slug" | tr '[:upper:]' '[:lower:]' | tr -cd 'a-z0-9_-')"
  [[ -n "$host_slug" ]] || host_slug="agent"
  MONITOR_ID="agent.${host_slug}"
  MONITOR_NAME="$(hostname -s 2>/dev/null || printf 'System agent')"
  MONITOR_HOST=""
  MONITOR_INTERVAL="60"
  MONITOR_TIMEOUT="5"
  MONITOR_SLOW_MS="500"

  if [[ -r "$MONITOR_META_FILE" ]]; then
    while IFS='=' read -r key value; do
      case "$key" in
        MONITOR_ID) MONITOR_ID="$value" ;;
        MONITOR_NAME) MONITOR_NAME="$value" ;;
        MONITOR_HOST) MONITOR_HOST="$value" ;;
        MONITOR_INTERVAL) MONITOR_INTERVAL="$value" ;;
        MONITOR_TIMEOUT) MONITOR_TIMEOUT="$value" ;;
        MONITOR_SLOW_MS) MONITOR_SLOW_MS="$value" ;;
      esac
    done < "$MONITOR_META_FILE"
  fi
}

generate_token() {
  od -An -N24 -tx1 /dev/urandom | tr -d ' \n'
}

validate_configuration() {
  [[ "$LISTEN_ADDRESS" =~ ^[0-9A-Fa-f:.]+$ ]] || fail "Listen address must be an IPv4 or IPv6 address."
  [[ "$LISTEN_PORT" =~ ^[0-9]{1,5}$ ]] || fail "Port must be a number between 1024 and 65535."
  local numeric_port=$((10#$LISTEN_PORT))
  (( numeric_port >= 1024 && numeric_port <= 65535 )) || fail "Port must be between 1024 and 65535."
  LISTEN_PORT="$numeric_port"
  if [[ -n "$ACCESS_TOKEN" ]]; then
    [[ "$ACCESS_TOKEN" =~ ^[A-Za-z0-9_-]{16,128}$ ]] || fail "Token must be 16-128 letters, digits, underscores, or hyphens."
  fi
  for setting in "$ENABLE_CPU" "$ENABLE_MEMORY" "$ENABLE_LOAD" "$ENABLE_NETWORK"; do
    [[ "$setting" == "0" || "$setting" == "1" ]] || fail "Metric enable settings must be 1 or 0."
  done
  for setting in "$WARN_CPU_PERCENT" "$WARN_MEMORY_PERCENT" "$WARN_IOWAIT_PERCENT" "$WARN_NIC_ERROR_DROP_PERCENT"; do
    if ! valid_percent_threshold "$setting"; then
      fail "Status warning thresholds must be greater than 0 and at most 100 percent, or 'off'."
    fi
  done
  [[ "$STATUS_NETWORK_MIN_PACKETS" =~ ^[1-9][0-9]{0,8}$ ]] || fail "Minimum network packets must be an integer from 1 to 999999999."
}

prompt_toggle() {
  local setting_name="$1"
  local label="$2"
  local current="${!setting_name}"
  local answer value
  if [[ "$current" == "1" ]]; then
    read -r -p "$label [Y/n]: " answer
  else
    read -r -p "$label [y/N]: " answer
  fi
  case "${answer,,}" in
    "") value="$current" ;;
    y|yes|1|on) value="1" ;;
    n|no|0|off) value="0" ;;
    *) fail "Choose yes or no for: $label" ;;
  esac
  printf -v "$setting_name" '%s' "$value"
}

prompt_status_threshold() {
  local setting_name="$1"
  local label="$2"
  local current="${!setting_name}"
  local answer value
  read -r -p "$label percent [${current}] (off disables): " answer
  value="${answer:-$current}"
  if [[ "${value,,}" == "off" ]]; then
    value="off"
  elif ! valid_percent_threshold "$value"; then
    fail "$label must be greater than 0 and at most 100 percent, or 'off'."
  fi
  printf -v "$setting_name" '%s' "$value"
}

valid_percent_threshold() {
  local value="${1,,}"
  [[ "$value" == "off" ]] && return 0
  awk -v threshold="$value" 'BEGIN {
    if (threshold !~ /^[0-9]+([.][0-9]+)?$/) exit 1
    value = threshold + 0
    if (value <= 0 || value > 100) exit 1
  }'
}

prompt_positive_integer() {
  local setting_name="$1"
  local label="$2"
  local current="${!setting_name}"
  local answer value
  read -r -p "$label [${current}]: " answer
  value="${answer:-$current}"
  [[ "$value" =~ ^[1-9][0-9]{0,8}$ ]] || fail "$label must be an integer from 1 to 999999999."
  printf -v "$setting_name" '%s' "$value"
}

show_monitor_config() {
  local monitor_send
  if [[ -n "$ACCESS_TOKEN" ]]; then
    monitor_send="METRICS ${ACCESS_TOKEN}"$'\n'
  else
    monitor_send="METRICS"$'\n'
  fi

  printf '\nComplete TCP monitor entry for the status server (paste below monitors:):\n'
  printf '```yaml\n'
  printf '  - id: %s\n' "$(yaml_quote "$MONITOR_ID")"
  printf '    name: %s\n' "$(yaml_quote "$MONITOR_NAME")"
  printf '    type: tcp\n'
  printf '    host: %s\n' "$(yaml_quote "$MONITOR_HOST")"
  printf '    port: %s\n' "$LISTEN_PORT"
  printf '    interval: %s\n' "$MONITOR_INTERVAL"
  printf '    timeout: %s\n' "$MONITOR_TIMEOUT"
  printf '    slow_ms: %s\n' "$MONITOR_SLOW_MS"
  printf '    hide_target: true\n'
  printf '    exclude_status: false\n'
  printf '    send: %s\n' "$(yaml_quote "$monitor_send")"
  printf '    max_bytes: 16384\n'
  printf '    response_codes:\n'
  printf '      0: operational\n'
  printf '      1: degraded\n'
  printf '      2: down\n'
  printf '```\n'
  printf 'No response_end is set; the agent closes each connection after sending its metrics.\n'
  printf 'Enabled metrics: CPU=%s memory=%s load=%s network=%s\n' \
    "$ENABLE_CPU" "$ENABLE_MEMORY" "$ENABLE_LOAD" "$ENABLE_NETWORK"
  printf 'Status warnings: CPU=%s%% memory=%s%% I/O wait=%s%% NIC error/drop=%s%% (minimum %s samples)\n' \
    "$WARN_CPU_PERCENT" "$WARN_MEMORY_PERCENT" "$WARN_IOWAIT_PERCENT" \
    "$WARN_NIC_ERROR_DROP_PERCENT" "$STATUS_NETWORK_MIN_PACKETS"
}

yaml_quote() {
  local value="$1"
  value="${value//\\/\\\\}"
  value="${value//\"/\\\"}"
  value="${value//$'\t'/\\t}"
  value="${value//$'\r'/\\r}"
  value="${value//$'\n'/\\n}"
  printf '"%s"' "$value"
}

add_host_candidate() {
  local address="$1"
  local label="$2"
  [[ -n "$address" ]] || return 0
  [[ "$address" =~ ^[0-9A-Fa-f:.]+$ ]] || return 0
  [[ "$address" == *:* || "$address" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]] || return 0
  case "$address" in
    0.0.0.0|127.*|169.254.*|::|::1|fe80:*) return 0 ;;
  esac
  local existing
  if ((${#HOST_CANDIDATE_ADDRESSES[@]})); then
    for existing in "${HOST_CANDIDATE_ADDRESSES[@]}"; do
      [[ "$existing" == "$address" ]] && return 0
    done
  fi
  HOST_CANDIDATE_ADDRESSES+=("$address")
  HOST_CANDIDATE_LABELS+=("$label")
}

is_virtual_interface() {
  case "$1" in
    docker*|br-*|veth*|virbr*|podman*|cni*|flannel*|lo) return 0 ;;
    *) return 1 ;;
  esac
}

detect_host_candidates() {
  HOST_CANDIDATE_ADDRESSES=()
  HOST_CANDIDATE_LABELS=()

  if command -v ip >/dev/null 2>&1; then
    local route_output route_address="" route_interface="" index
    route_output="$(ip -4 route get 1.1.1.1 2>/dev/null | head -n 1 || true)"
    local -a route_fields=($route_output)
    for ((index = 0; index < ${#route_fields[@]}; index++)); do
      case "${route_fields[$index]}" in
        src) route_address="${route_fields[$((index + 1))]:-}" ;;
        dev) route_interface="${route_fields[$((index + 1))]:-}" ;;
      esac
    done
    if [[ -n "$route_address" ]]; then
      add_host_candidate "$route_address" "default route${route_interface:+ via $route_interface}"
    fi

    local interface family address rest
    while read -r _ interface family address rest; do
      [[ "$family" == "inet" ]] || continue
      is_virtual_interface "$interface" && continue
      add_host_candidate "${address%%/*}" "$interface"
    done < <(ip -o -4 addr show scope global 2>/dev/null || true)
    while read -r _ interface family address rest; do
      [[ "$family" == "inet6" ]] || continue
      is_virtual_interface "$interface" && continue
      add_host_candidate "${address%%/*}" "$interface"
    done < <(ip -o -6 addr show scope global 2>/dev/null || true)
  fi

  if ((${#HOST_CANDIDATE_ADDRESSES[@]} == 0)) && command -v hostname >/dev/null 2>&1; then
    local address
    for address in $(hostname -I 2>/dev/null || true); do
      add_host_candidate "$address" "hostname -I"
    done
  fi
}

validate_monitor_host() {
  local host="$1"
  [[ -n "$host" && "$host" =~ ^[A-Za-z0-9._:-]+$ ]] || return 1
  getent ahosts "$host" >/dev/null 2>&1
}

choose_monitor_host() {
  detect_host_candidates
  printf '\nChoose the address the status server can reach.\n'
  local default_choice="1" index choice answer
  [[ -z "$MONITOR_HOST" ]] || default_choice="m"
  if ((${#HOST_CANDIDATE_ADDRESSES[@]})); then
    for index in "${!HOST_CANDIDATE_ADDRESSES[@]}"; do
      printf '  %s) %s (%s)\n' "$((index + 1))" "${HOST_CANDIDATE_ADDRESSES[$index]}" "${HOST_CANDIDATE_LABELS[$index]}"
      if [[ -n "$MONITOR_HOST" && "$MONITOR_HOST" == "${HOST_CANDIDATE_ADDRESSES[$index]}" ]]; then
        default_choice="$((index + 1))"
      fi
    done
  else
    default_choice="m"
    printf '  No local IP candidates were detected.\n'
  fi
  printf '  m) Enter an IP address or DNS name manually\n'

  read -r -p "Host address [${default_choice}]: " choice
  choice="${choice:-$default_choice}"
  if [[ "$choice" =~ ^[0-9]+$ ]] && ((choice >= 1 && choice <= ${#HOST_CANDIDATE_ADDRESSES[@]})); then
    MONITOR_HOST="${HOST_CANDIDATE_ADDRESSES[$((choice - 1))]}"
  elif [[ "${choice,,}" == "m" ]]; then
    read -r -p "Reachable agent IP or DNS name [${MONITOR_HOST}]: " answer
    MONITOR_HOST="${answer:-$MONITOR_HOST}"
    validate_monitor_host "$MONITOR_HOST" || fail "Enter a valid IP address or a DNS name resolvable on this host."
  else
    fail "Choose a listed address number or 'm'."
  fi
}

prompt_monitor_identity() {
  local answer
  read -r -p "Monitor ID (dot-separated group ID) [${MONITOR_ID}]: " answer
  MONITOR_ID="${answer:-$MONITOR_ID}"
  [[ "$MONITOR_ID" =~ ^[A-Za-z0-9_-]+(\.[A-Za-z0-9_-]+)*$ ]] || fail "Monitor ID must contain dot-separated ASCII letters, digits, '_' or '-'."
  ((${#MONITOR_ID} <= 128)) || fail "Monitor ID must be 128 characters or fewer."

  read -r -p "Monitor display name [${MONITOR_NAME}]: " answer
  MONITOR_NAME="${answer:-$MONITOR_NAME}"
  [[ -n "${MONITOR_NAME//[[:space:]]/}" ]] || fail "Monitor display name must not be empty."
  if LC_ALL=C grep -q '[[:cntrl:]]' < <(printf '%s' "$MONITOR_NAME"); then
    fail "Monitor display name must not contain control characters."
  fi
}

write_monitor_metadata() {
  install -d -m 0750 "$CONFIG_DIR"
  {
    printf 'MONITOR_ID=%s\n' "$MONITOR_ID"
    printf 'MONITOR_NAME=%s\n' "$MONITOR_NAME"
    printf 'MONITOR_HOST=%s\n' "$MONITOR_HOST"
    printf 'MONITOR_INTERVAL=%s\n' "$MONITOR_INTERVAL"
    printf 'MONITOR_TIMEOUT=%s\n' "$MONITOR_TIMEOUT"
    printf 'MONITOR_SLOW_MS=%s\n' "$MONITOR_SLOW_MS"
  } > "$MONITOR_META_FILE"
  chmod 0600 "$MONITOR_META_FILE"
}

configure() {
  require_root "$@"
  require_linux_systemd
  load_config
  load_monitor_metadata
  local default_token="$ACCESS_TOKEN"
  [[ -n "$default_token" ]] || default_token="$(generate_token)"

  local answer
  read -r -p "Listen address [${LISTEN_ADDRESS}]: " answer
  LISTEN_ADDRESS="${answer:-$LISTEN_ADDRESS}"
  read -r -p "TCP port [${LISTEN_PORT}]: " answer
  LISTEN_PORT="${answer:-$LISTEN_PORT}"
  read -r -p "Access token [${default_token}] (enter 'none' to disable): " answer
  ACCESS_TOKEN="${answer:-$default_token}"
  [[ "$ACCESS_TOKEN" == "none" ]] && ACCESS_TOKEN=""

  prompt_toggle ENABLE_CPU "Enable aggregate/per-core CPU usage and I/O wait"
  prompt_toggle ENABLE_MEMORY "Enable memory metrics"
  prompt_toggle ENABLE_LOAD "Enable load average metrics"
  prompt_toggle ENABLE_NETWORK "Enable per-interface network metrics"
  prompt_status_threshold WARN_CPU_PERCENT "Warn at CPU usage"
  prompt_status_threshold WARN_MEMORY_PERCENT "Warn at memory usage"
  prompt_status_threshold WARN_IOWAIT_PERCENT "Warn at I/O wait"
  prompt_status_threshold WARN_NIC_ERROR_DROP_PERCENT "Warn at NIC error/drop ratio"
  prompt_positive_integer STATUS_NETWORK_MIN_PACKETS "Minimum packet/error/drop samples for NIC evaluation"

  prompt_monitor_identity
  choose_monitor_host

  validate_configuration
  install -d -m 0750 "$CONFIG_DIR"
  {
    printf 'WS371_AGENT_BIND=%s\n' "$LISTEN_ADDRESS"
    printf 'WS371_AGENT_PORT=%s\n' "$LISTEN_PORT"
    printf 'WS371_AGENT_TOKEN=%s\n' "$ACCESS_TOKEN"
    printf 'WS371_AGENT_ENABLE_CPU=%s\n' "$ENABLE_CPU"
    printf 'WS371_AGENT_ENABLE_MEMORY=%s\n' "$ENABLE_MEMORY"
    printf 'WS371_AGENT_ENABLE_LOAD=%s\n' "$ENABLE_LOAD"
    printf 'WS371_AGENT_ENABLE_NETWORK=%s\n' "$ENABLE_NETWORK"
    printf 'WS371_AGENT_WARN_CPU_PERCENT=%s\n' "$WARN_CPU_PERCENT"
    printf 'WS371_AGENT_WARN_MEMORY_PERCENT=%s\n' "$WARN_MEMORY_PERCENT"
    printf 'WS371_AGENT_WARN_IOWAIT_PERCENT=%s\n' "$WARN_IOWAIT_PERCENT"
    printf 'WS371_AGENT_WARN_NIC_ERROR_DROP_PERCENT=%s\n' "$WARN_NIC_ERROR_DROP_PERCENT"
    printf 'WS371_AGENT_STATUS_NETWORK_MIN_PACKETS=%s\n' "$STATUS_NETWORK_MIN_PACKETS"
  } > "$CONFIG_FILE"
  chmod 0600 "$CONFIG_FILE"
  write_monitor_metadata

  if systemctl cat "$SERVICE_NAME.service" >/dev/null 2>&1; then
    systemctl restart "$SERVICE_NAME.service"
  fi
  printf 'Configuration saved to %s\n' "$CONFIG_FILE"
  [[ "$CONFIGURE_FROM_INSTALL" == "1" ]] || show_monitor_config
}

find_binary() {
  if [[ -x "${SCRIPT_DIR}/${BINARY_NAME}" ]]; then
    SOURCE_BINARY="${SCRIPT_DIR}/${BINARY_NAME}"
    return
  fi
  if [[ -x "${WORKSPACE_ROOT}/target/release/${BINARY_NAME}" ]]; then
    SOURCE_BINARY="${WORKSPACE_ROOT}/target/release/${BINARY_NAME}"
    return
  fi

  TEMP_BINARY="$(mktemp)" || fail "Could not create a temporary download file."
  if command -v curl >/dev/null 2>&1; then
    curl --fail --location --retry 3 --output "$TEMP_BINARY" "$RELEASE_BINARY_URL" \
      || fail "Could not download the latest agent binary from GitHub Releases."
  elif command -v wget >/dev/null 2>&1; then
    wget --output-document="$TEMP_BINARY" "$RELEASE_BINARY_URL" \
      || fail "Could not download the latest agent binary from GitHub Releases."
  else
    fail "Install curl or wget to download the agent binary from GitHub Releases."
  fi
  [[ -s "$TEMP_BINARY" ]] || fail "The downloaded agent binary is empty."
  chmod 0755 "$TEMP_BINARY"
  SOURCE_BINARY="$TEMP_BINARY"
}

install_service() {
  require_root "$@"
  require_linux_systemd
  find_binary
  local source_binary="$SOURCE_BINARY"

  if ! getent passwd "$SERVICE_USER" >/dev/null 2>&1; then
    useradd --system --user-group --home-dir /nonexistent --no-create-home --shell /usr/sbin/nologin "$SERVICE_USER"
  fi

  install -o root -g root -m 0755 "$source_binary" "$INSTALL_PATH"
  if [[ ! -f "$CONFIG_FILE" || ! -r "$MONITOR_META_FILE" ]]; then
    CONFIGURE_FROM_INSTALL="1"
    configure
    CONFIGURE_FROM_INSTALL="0"
  fi

  cat > "$UNIT_FILE" <<'UNIT'
[Unit]
Description=wk-status-371tti-net system metrics agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=wkstatus-agent
Group=wkstatus-agent
EnvironmentFile=/etc/wk-status-371tti-net-agent/agent.env
ExecStart=/usr/local/bin/wk-status-371tti-net-agent
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX
LockPersonality=true
MemoryDenyWriteExecute=true
UMask=0077

[Install]
WantedBy=multi-user.target
UNIT

  chmod 0644 "$UNIT_FILE"
  systemctl daemon-reload
  systemctl enable --now "$SERVICE_NAME.service"
  load_config
  load_monitor_metadata
  printf 'Installed and started %s.service\n' "$SERVICE_NAME"
  show_monitor_config
}

uninstall_service() {
  local purge="${1:-}"
  require_root "$@"
  require_linux_systemd
  systemctl disable --now "$SERVICE_NAME.service" >/dev/null 2>&1 || true
  rm -f "$UNIT_FILE" "$INSTALL_PATH"
  systemctl daemon-reload
  if getent passwd "$SERVICE_USER" >/dev/null 2>&1; then
    userdel "$SERVICE_USER" || true
  fi
  if [[ "$purge" == "--purge" ]]; then
    rm -f "$CONFIG_FILE" "$MONITOR_META_FILE"
    rmdir "$CONFIG_DIR" 2>/dev/null || true
    printf 'Removed service, binary, and configuration.\n'
  else
    printf 'Removed service and binary. Configuration is preserved at %s.\n' "$CONFIG_FILE"
  fi
}

main() {
  local command="${1:-}"
  case "$command" in
    install)
      shift
      install_service "$@"
      ;;
    configure)
      shift
      configure "$@"
      ;;
    uninstall)
      shift
      [[ $# -le 1 && ( $# -eq 0 || "$1" == "--purge" ) ]] || fail "uninstall accepts only --purge."
      uninstall_service "${1:-}"
      ;;
    -h|--help|help)
      usage
      ;;
    *)
      usage >&2
      exit 2
      ;;
  esac
}

main "$@"
