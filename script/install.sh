#!/bin/sh
set -eu

# Mentenaz Forge Super - Debian/Ubuntu/Kali repository installer
#
# Usage:
#   curl -fsSL https://downloads.mentenaz-server.com/install.sh | sudo sh
#   curl -fsSL https://downloads.mentenaz-server.com/install.sh | sudo sh -s -- --repository-only
#
# IMPORTANT:
# Set EXPECTED_KEY_FINGERPRINT to the full fingerprint of the public signing key
# used by the Mentenaz APT repository before publishing this script.

REPO_URL="https://downloads.mentenaz-server.com"
REPO_DISTRIBUTION="stable"
REPO_COMPONENT="main"
REPO_KEY_URL="${REPO_URL}/mentenaz.gpg"
KEYRING_PATH="/usr/share/keyrings/mentenaz-archive-keyring.gpg"
SOURCE_LIST_PATH="/etc/apt/sources.list.d/mentenaz.list"
PACKAGE_NAME="forge-super"

# Replace this with the full signing-key fingerprint (40 hexadecimal characters).
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
    REPLACE_*|"")
        fail "The installer has not been configured with the Mentenaz signing-key fingerprint. Set EXPECTED_KEY_FINGERPRINT before publishing."
        ;;
esac

case "$EXPECTED_KEY_FINGERPRINT" in
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






# #!/usr/bin/env sh
# set -eu
#
# # Downloads a tarball from https://zed.dev/releases and unpacks it
# # into ~/.local/. If you'd prefer to do this manually, instructions are at
# # https://zed.dev/docs/linux.
#
# main() {
#     platform="$(uname -s)"
#     arch="$(uname -m)"
#     channel="${ZED_CHANNEL:-stable}"
#     ZED_VERSION="${ZED_VERSION:-latest}"
#     # Use TMPDIR if available (for environments with non-standard temp directories)
#     if [ -n "${TMPDIR:-}" ] && [ -d "${TMPDIR}" ]; then
#         temp="$(mktemp -d "$TMPDIR/zed-XXXXXX")"
#     else
#         temp="$(mktemp -d "/tmp/zed-XXXXXX")"
#     fi
#
#     if [ "$platform" = "Darwin" ]; then
#         platform="macos"
#     elif [ "$platform" = "Linux" ]; then
#         platform="linux"
#     else
#         echo "Unsupported platform $platform"
#         exit 1
#     fi
#
#     case "$platform-$arch" in
#         macos-arm64* | linux-arm64* | linux-aarch64)
#             arch="aarch64"
#             ;;
#         macos-x86* | linux-x86*)
#             arch="x86_64"
#             ;;
#         *)
#             echo "Unsupported platform or architecture"
#             exit 1
#             ;;
#     esac
#
#     if command -v curl >/dev/null 2>&1; then
#         curl () {
#             command curl -fL "$@"
#         }
#     elif command -v wget >/dev/null 2>&1; then
#         curl () {
#             wget -O- "$@"
#         }
#     else
#         echo "Could not find 'curl' or 'wget' in your path"
#         exit 1
#     fi
#
#     "$platform" "$@"
#
#     if [ "$(command -v zed)" = "$HOME/.local/bin/zed" ]; then
#         echo "Zed has been installed. Run with 'zed'"
#     else
#         echo "To run Zed from your terminal, you must add ~/.local/bin to your PATH"
#         echo "Run:"
#
#         case "$SHELL" in
#             *zsh)
#                 echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.zshrc"
#                 echo "   source ~/.zshrc"
#                 ;;
#             *fish)
#                 echo "   fish_add_path -U $HOME/.local/bin"
#                 ;;
#             *)
#                 echo "   echo 'export PATH=\$HOME/.local/bin:\$PATH' >> ~/.bashrc"
#                 echo "   source ~/.bashrc"
#                 ;;
#         esac
#
#         echo "To run Zed now, '~/.local/bin/zed'"
#     fi
# }
#
# linux() {
#     if [ -n "${ZED_BUNDLE_PATH:-}" ]; then
#         cp "$ZED_BUNDLE_PATH" "$temp/zed-linux-$arch.tar.gz"
#     else
#         echo "Downloading Zed version: $ZED_VERSION"
#         curl "https://cloud.zed.dev/releases/$channel/$ZED_VERSION/download?asset=zed&arch=$arch&os=linux&source=install.sh" > "$temp/zed-linux-$arch.tar.gz"
#     fi
#
#     suffix=""
#     if [ "$channel" != "stable" ]; then
#         suffix="-$channel"
#     fi
#
#     appid=""
#     case "$channel" in
#       stable)
#         appid="dev.zed.Zed"
#         ;;
#       nightly)
#         appid="dev.zed.Zed-Nightly"
#         ;;
#       preview)
#         appid="dev.zed.Zed-Preview"
#         ;;
#       dev)
#         appid="dev.zed.Zed-Dev"
#         ;;
#       *)
#         echo "Unknown release channel: ${channel}. Using stable app ID."
#         appid="dev.zed.Zed"
#         ;;
#     esac
#
#     # Unpack
#     rm -rf "$HOME/.local/zed$suffix.app"
#     mkdir -p "$HOME/.local/zed$suffix.app"
#     tar -xzf "$temp/zed-linux-$arch.tar.gz" -C "$HOME/.local/"
#
#     zed_editor="$HOME/.local/zed$suffix.app/libexec/zed-editor"
#     if [ -f "$zed_editor" ] && command -v ldd >/dev/null 2>&1; then
#         missing="$(ldd "$zed_editor" 2>/dev/null | sed -n 's/^[[:space:]]*\(.*\) => not found$/\1/p')"
#         if [ -n "$missing" ]; then
#             echo "Warning: your system is missing libraries that Zed needs:"
#             echo "$missing" | sed 's/^/    /'
#             echo "Install them with your package manager, or Zed will fail to start."
#         fi
#     fi
#
#     # Setup ~/.local directories
#     mkdir -p "$HOME/.local/bin" "$HOME/.local/share/applications"
#
#     # Link the binary
#     if [ -f "$HOME/.local/zed$suffix.app/bin/zed" ]; then
#         ln -sf "$HOME/.local/zed$suffix.app/bin/zed" "$HOME/.local/bin/zed"
#     else
#         # support for versions before 0.139.x.
#         ln -sf "$HOME/.local/zed$suffix.app/bin/cli" "$HOME/.local/bin/zed"
#     fi
#
#     # Copy .desktop file
#     desktop_file_path="$HOME/.local/share/applications/${appid}.desktop"
#     src_dir="$HOME/.local/zed$suffix.app/share/applications"
#     if [ -f "$src_dir/${appid}.desktop" ]; then
#         cp "$src_dir/${appid}.desktop" "${desktop_file_path}"
#     else
#         # Fallback for older tarballs
#         cp "$src_dir/zed$suffix.desktop" "${desktop_file_path}"
#     fi
#     sed -i "s|Icon=zed|Icon=$HOME/.local/zed$suffix.app/share/icons/hicolor/512x512/apps/zed.png|g" "${desktop_file_path}"
#     sed -i "s|Exec=zed|Exec=$HOME/.local/zed$suffix.app/bin/zed|g" "${desktop_file_path}"
# }
#
# macos() {
#     echo "Downloading Zed version: $ZED_VERSION"
#     curl "https://cloud.zed.dev/releases/$channel/$ZED_VERSION/download?asset=zed&os=macos&arch=$arch&source=install.sh" > "$temp/Zed-$arch.dmg"
#     hdiutil attach -quiet "$temp/Zed-$arch.dmg" -mountpoint "$temp/mount"
#     app="$(cd "$temp/mount/"; echo *.app)"
#     echo "Installing $app"
#     if [ -d "/Applications/$app" ]; then
#         echo "Removing existing $app"
#         rm -rf "/Applications/$app"
#     fi
#     ditto "$temp/mount/$app" "/Applications/$app"
#     hdiutil detach -quiet "$temp/mount"
#
#     mkdir -p "$HOME/.local/bin"
#     # Link the binary
#     ln -sf "/Applications/$app/Contents/MacOS/cli" "$HOME/.local/bin/zed"
# }
#
# main "$@"
