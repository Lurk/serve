#!/usr/bin/env bash
set -euo pipefail

BINARY_NAME="serve"
SERVICE_NAME="serve.service"
CONFIG_DIR="/etc/serve"
CONFIG_FILE="${CONFIG_DIR}/serve.toml"
LOG_DIR="/var/log/serve"
CONTENT_DIR="/var/www/serve"
INSTALL_BIN="/usr/local/bin/${BINARY_NAME}"
UNIT_FILE="/etc/systemd/system/${SERVICE_NAME}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

usage() {
    echo "Usage: $0 [install|uninstall] [--binary <path>]"
    echo ""
    echo "Commands:"
    echo "  install      Install serve as a systemd service (default)"
    echo "  uninstall    Remove the serve service and all installed files"
    echo ""
    echo "Options:"
    echo "  --binary <path>   Use a pre-built binary instead of building with cargo"
    exit 1
}

check_root() {
    if [ "$(id -u)" -ne 0 ]; then
        echo "Error: This script must be run as root."
        exit 1
    fi
}

do_install() {
    local binary_path=""

    while [[ $# -gt 0 ]]; do
        case "$1" in
            --binary)
                binary_path="$2"
                shift 2
                ;;
            *)
                echo "Unknown option: $1"
                usage
                ;;
        esac
    done

    check_root

    # Build or locate the binary
    if [ -n "$binary_path" ]; then
        if [ ! -f "$binary_path" ]; then
            echo "Error: Binary not found at ${binary_path}"
            exit 1
        fi
        echo "Using pre-built binary: ${binary_path}"
    else
        echo "Building serve with cargo..."
        cargo build --release --manifest-path "${SCRIPT_DIR}/Cargo.toml"
        binary_path="${SCRIPT_DIR}/target/release/${BINARY_NAME}"
    fi

    # Create serve system user
    if ! id -u "$BINARY_NAME" >/dev/null 2>&1; then
        echo "Creating system user '${BINARY_NAME}'..."
        useradd --system --no-create-home --shell /usr/sbin/nologin "$BINARY_NAME"
    else
        echo "User '${BINARY_NAME}' already exists."
    fi

    # Install binary
    echo "Installing binary to ${INSTALL_BIN}..."
    install -m 755 "$binary_path" "$INSTALL_BIN"

    # Create config directory and default config
    if [ ! -f "$CONFIG_FILE" ]; then
        echo "Creating default config at ${CONFIG_FILE}..."
        mkdir -p "$CONFIG_DIR"
        cat > "$CONFIG_FILE" <<'TOML'
# Configuration for Serve (https://github.com/Lurk/serve)

path = "/var/www/serve"
port = 3000
addr = "127.0.0.1"
disable_compression = false
ok = false
log_level = "info"
log_path = "/var/log/serve"
log_max_files = 7
TOML
    else
        echo "Config already exists at ${CONFIG_FILE}, skipping."
    fi

    # Create log directory
    echo "Creating log directory ${LOG_DIR}..."
    mkdir -p "$LOG_DIR"
    chown "$BINARY_NAME":"$BINARY_NAME" "$LOG_DIR"

    # Create content directory
    echo "Creating content directory ${CONTENT_DIR}..."
    mkdir -p "$CONTENT_DIR"
    chown "$BINARY_NAME":"$BINARY_NAME" "$CONTENT_DIR"

    # Install systemd unit file
    echo "Installing systemd unit file..."
    install -m 644 "${SCRIPT_DIR}/${SERVICE_NAME}" "$UNIT_FILE"

    # Reload systemd, enable and start
    echo "Reloading systemd..."
    systemctl daemon-reload

    echo "Enabling and starting ${SERVICE_NAME}..."
    systemctl enable "$SERVICE_NAME"
    systemctl start "$SERVICE_NAME"

    echo ""
    echo "serve has been installed and started."
    echo "  Config:  ${CONFIG_FILE}"
    echo "  Logs:    ${LOG_DIR}"
    echo "  Content: ${CONTENT_DIR}"
    echo ""
    echo "Manage with:"
    echo "  systemctl status ${SERVICE_NAME}"
    echo "  systemctl stop ${SERVICE_NAME}"
    echo "  systemctl restart ${SERVICE_NAME}"
    echo "  journalctl -u ${SERVICE_NAME}"
}

do_uninstall() {
    check_root

    echo "Uninstalling serve..."

    # Stop and disable service
    if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
        echo "Stopping ${SERVICE_NAME}..."
        systemctl stop "$SERVICE_NAME"
    fi
    if systemctl is-enabled --quiet "$SERVICE_NAME" 2>/dev/null; then
        echo "Disabling ${SERVICE_NAME}..."
        systemctl disable "$SERVICE_NAME"
    fi

    # Remove unit file
    if [ -f "$UNIT_FILE" ]; then
        echo "Removing unit file..."
        rm "$UNIT_FILE"
        systemctl daemon-reload
    fi

    # Remove binary
    if [ -f "$INSTALL_BIN" ]; then
        echo "Removing binary..."
        rm "$INSTALL_BIN"
    fi

    # Remove user
    if id -u "$BINARY_NAME" >/dev/null 2>&1; then
        echo "Removing system user '${BINARY_NAME}'..."
        userdel "$BINARY_NAME"
    fi

    echo ""
    echo "serve has been uninstalled."
    echo ""
    echo "The following directories were preserved (remove manually if desired):"
    echo "  ${CONFIG_DIR}"
    echo "  ${LOG_DIR}"
    echo "  ${CONTENT_DIR}"
}

# Parse command
COMMAND="${1:-install}"
shift || true

case "$COMMAND" in
    install)
        do_install "$@"
        ;;
    uninstall)
        do_uninstall "$@"
        ;;
    *)
        usage
        ;;
esac
