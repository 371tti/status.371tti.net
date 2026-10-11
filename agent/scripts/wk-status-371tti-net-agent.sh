#!/usr/bin/env bash
set -Eeuo pipefail

SERVICE_NAME="wk-status-371tti-net-agent"
SERVICE_USER="wkstatus-agent"
BINARY_NAME="wk-status-371tti-net-agent"
RELEASE_BINARY_URL="https://github.com/371tti/status.371tti.net/releases/latest/download/${BINARY_NAME}"
CONFIG_DIR="/etc/${SERVICE_NAME}"
CONFIG_FILE="${CONFIG_DIR}/agent.env"
UNIT_FILE="/etc/systemd/system/${SERVICE_NAME}.service"
INSTALL_PATH="/usr/local/bin/${BINARY_NAME}"
SCRIPT_PATH="$(readlink -f "${BASH_SOURCE[0]}")"
SCRIPT_DIR="$(cd "$(dirname "$SCRIPT_PATH")" && pwd)"
WORKSPACE_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
ORIGINAL_ARGS=("$@")
TEMP_BINARY=""
SOURCE_BINARY=""

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
  configure    Set the listener, access token, and enabled metric groups.
  uninstall    Remove the service and binary; keep configuration by default.
  --purge      Also remove the configuration and generated token.
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
      esac
    done < "$CONFIG_FILE"
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

show_monitor_config() {
  printf '\nUse this TCP monitor configuration on the status server:\n'
  printf '  type: tcp\n'
  printf '  host: <agent-host-address>\n'
  printf '  port: %s\n' "$LISTEN_PORT"
  if [[ -n "$ACCESS_TOKEN" ]]; then
    printf '  send: "METRICS %s\\n"\n' "$ACCESS_TOKEN"
  else
    printf '  send: "METRICS\\n"\n'
  fi
  printf '  slow_ms: 500\n'
  printf 'Do not set response_end; the agent closes the connection after the full response.\n'
  printf 'Enabled metrics: CPU=%s memory=%s load=%s network=%s\n' \
    "$ENABLE_CPU" "$ENABLE_MEMORY" "$ENABLE_LOAD" "$ENABLE_NETWORK"
}

configure() {
  require_root "$@"
  require_linux_systemd
  load_config
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
  } > "$CONFIG_FILE"
  chmod 0600 "$CONFIG_FILE"

  if systemctl cat "$SERVICE_NAME.service" >/dev/null 2>&1; then
    systemctl restart "$SERVICE_NAME.service"
  fi
  printf 'Configuration saved to %s\n' "$CONFIG_FILE"
  show_monitor_config
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
  if [[ ! -f "$CONFIG_FILE" ]]; then
    configure
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
    rm -f "$CONFIG_FILE"
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
