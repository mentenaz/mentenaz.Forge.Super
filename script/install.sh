#!/bin/sh
set -eu

# Mentenaz Forge Super - Debian/Ubuntu/Kali repository installer
#
# Usage:
#   curl -fsSL https://downloads.mentenaz-server.com/install.sh | sudo sh
#   curl -fsSL https://downloads.mentenaz-server.com/install.sh | sudo sh -s -- --repository-only
#
# Published next to the apt repository by build_installers.yml (it is copied
# into the tree that gets uploaded), so it always matches the repository it
# configures.

REPO_URL="https://downloads.mentenaz-server.com"
REPO_DISTRIBUTION="stable"
REPO_COMPONENT="main"
REPO_KEY_URL="${REPO_URL}/mentenaz.gpg"
KEYRING_PATH="/usr/share/keyrings/mentenaz-archive-keyring.gpg"
SOURCE_LIST_PATH="/etc/apt/sources.list.d/mentenaz.list"
PACKAGE_NAME="forge-super"

# Full fingerprint of the repository's signing key. The key is downloaded
# from the same server as the packages, so this pin is what makes the check
# meaningful: a key that does not match is rejected before anything is
# installed. Update it here (and republish this script) if the signing key
# is ever replaced.
EXPECTED_KEY_FINGERPRINT="42E9F3A7505F1AAD3DA6DCDFF128FBB1B8210F56"

REPOSITORY_ONLY=0

log() {
    printf '%s\n' "$*"
}

fail() {
    printf 'Error: %s\n' "$*" >&2
    exit 1
}

usage() {
    cat <<'EOF'
Mentenaz Forge Super installer

Usage:
  install.sh [--repository-only] [--help]

Options:
  --repository-only  Register the Mentenaz APT repository and update package
                     indexes, but do not install Forge Super.
  --help             Show this help.

This installer supports Debian-based distributions (including Ubuntu and Kali)
on amd64 systems.
EOF
}

# Everything runs from main(), called on the last line. When this script is
# piped into sh (`curl ... | sudo sh`), the shell reads and runs it as it
# arrives, so a command that reads standard input (apt-get or dpkg asking a
# question) could swallow the rest of the script, and a dropped connection
# would run half of it. Wrapping it means the whole script has to be received
# and parsed before the first command runs.
main() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --repository-only)
                REPOSITORY_ONLY=1
                ;;
            --help|-h)
                usage
                exit 0
                ;;
            *)
                fail "Unknown option: $1 (use --help for usage)"
                ;;
        esac
        shift
    done

    [ "$(id -u)" -eq 0 ] || fail "Run this installer as root, for example: curl -fsSL ${REPO_URL}/install.sh | sudo sh"

    [ -r /etc/os-release ] || fail "Cannot identify this Linux distribution (/etc/os-release is missing)."
    # shellcheck disable=SC1091
    . /etc/os-release

    case " ${ID:-} ${ID_LIKE:-} " in
        *" debian "*|*" ubuntu "*|*" kali "*)
            ;;
        *)
            fail "Unsupported distribution: ${PRETTY_NAME:-unknown}. Debian, Ubuntu and Kali are supported."
            ;;
    esac

    command -v apt-get >/dev/null 2>&1 || fail "apt-get was not found. This installer requires APT."

    architecture="$(dpkg --print-architecture 2>/dev/null || true)"
    [ "$architecture" = "amd64" ] || fail "Unsupported architecture: ${architecture:-unknown}. This installer currently supports amd64 only."

    case "$EXPECTED_KEY_FINGERPRINT" in
        "")
            fail "The installer has no signing-key fingerprint to check against."
            ;;
        *[!0-9A-Fa-f]*)
            fail "EXPECTED_KEY_FINGERPRINT must contain hexadecimal characters only."
            ;;
    esac

    [ "${#EXPECTED_KEY_FINGERPRINT}" -eq 40 ] || fail "EXPECTED_KEY_FINGERPRINT must be the full 40-character OpenPGP fingerprint."

    EXPECTED_KEY_FINGERPRINT="$(printf '%s' "$EXPECTED_KEY_FINGERPRINT" | tr '[:lower:]' '[:upper:]')"

    tmp_dir="$(mktemp -d)"
    cleanup() {
        rm -rf "$tmp_dir"
    }
    trap cleanup EXIT HUP INT TERM

    log "Preparing Mentenaz Forge Super repository on ${PRETTY_NAME:-this system}..."

    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y ca-certificates curl gnupg

    log "Downloading Mentenaz repository signing key..."
    curl --fail --silent --show-error --location \
        "$REPO_KEY_URL" \
        --output "$tmp_dir/mentenaz.gpg"

    log "Verifying signing-key fingerprint..."
    actual_fingerprint="$(
        gpg --batch --show-keys --with-colons "$tmp_dir/mentenaz.gpg" 2>/dev/null \
            | awk -F: '$1 == "fpr" { print toupper($10); exit }'
    )"

    [ -n "$actual_fingerprint" ] || fail "Could not read a signing-key fingerprint from ${REPO_KEY_URL}."

    [ "$actual_fingerprint" = "$EXPECTED_KEY_FINGERPRINT" ] || {
        printf 'Expected fingerprint: %s\n' "$EXPECTED_KEY_FINGERPRINT" >&2
        printf 'Received fingerprint: %s\n' "$actual_fingerprint" >&2
        fail "The downloaded signing key does not match the trusted Mentenaz fingerprint. No repository changes were made."
    }

    log "Installing repository key..."
    gpg --batch --yes --dearmor \
        --output "$tmp_dir/mentenaz-archive-keyring.gpg" \
        "$tmp_dir/mentenaz.gpg"

    install -d -m 0755 "$(dirname "$KEYRING_PATH")"
    install -m 0644 "$tmp_dir/mentenaz-archive-keyring.gpg" "$KEYRING_PATH"

    log "Registering APT repository..."
    printf 'deb [arch=%s signed-by=%s] %s %s %s\n' \
        "$architecture" \
        "$KEYRING_PATH" \
        "$REPO_URL" \
        "$REPO_DISTRIBUTION" \
        "$REPO_COMPONENT" \
        > "$tmp_dir/mentenaz.list"

    install -d -m 0755 "$(dirname "$SOURCE_LIST_PATH")"
    install -m 0644 "$tmp_dir/mentenaz.list" "$SOURCE_LIST_PATH"

    log "Refreshing APT package indexes..."
    apt-get update

    if [ "$REPOSITORY_ONLY" -eq 1 ]; then
        log "Repository configured successfully."
        log "Install Forge Super later with: sudo apt-get install ${PACKAGE_NAME}"
        exit 0
    fi

    log "Installing Forge Super..."
    apt-get install -y "$PACKAGE_NAME"

    log ""
    log "Mentenaz Forge Super has been installed."
    log "Launch it with: forge-super"
    log ""
    log "To update Forge Super in the future, run:"
    log "  sudo apt-get update && sudo apt-get upgrade"
}

main "$@"
