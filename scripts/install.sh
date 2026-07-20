#!/bin/bash

set -euo pipefail

readonly REPO="KKyosuke/usagi"
readonly USAGI_DIR="${USAGI_HOME:-$HOME/.usagi}"
readonly BIN_DIR="$USAGI_DIR/bin"
readonly TARGET="$BIN_DIR/usagi"
readonly LOCK_DIR="$USAGI_DIR/update.lock"

STAGE_DIR=""
LOCK_HELD=0

cleanup() {
    local status=$?
    if [ -n "$STAGE_DIR" ] && [ -d "$STAGE_DIR" ]; then
        rm -rf -- "$STAGE_DIR"
    fi
    if [ "$LOCK_HELD" -eq 1 ] && [ -d "$LOCK_DIR" ]; then
        rm -rf -- "$LOCK_DIR"
    fi
    exit "$status"
}
trap cleanup EXIT HUP INT TERM

fail() {
    echo "Error: $*" >&2
    exit 1
}

read_version() {
    local bin=$1 output
    [ -x "$bin" ] || return 0
    output="$($bin --version 2>/dev/null)" || return 0
    printf '%s\n' "$output" | awk 'NF == 2 && $1 == "usagi" { print $2 }'
}

acquire_lock() {
    local attempt=0 owner=""
    mkdir -p -- "$USAGI_DIR"
    chmod 700 "$USAGI_DIR"

    while ! mkdir -m 700 "$LOCK_DIR" 2>/dev/null; do
        if [ -f "$LOCK_DIR/pid" ]; then
            owner="$(sed -n '1p' "$LOCK_DIR/pid" 2>/dev/null || true)"
        fi
        case "$owner" in
            ''|*[!0-9]*) ;;
            *)
                if ! kill -0 "$owner" 2>/dev/null; then
                    rm -rf -- "$LOCK_DIR"
                    owner=""
                    continue
                fi
                ;;
        esac
        attempt=$((attempt + 1))
        [ "$attempt" -lt 600 ] || fail "another usagi update is still running"
        sleep 0.1
    done
    LOCK_HELD=1
    printf '%s\n' "$$" > "$LOCK_DIR/pid"
}

platform_asset() {
    local os arch
    os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    arch="$(uname -m)"
    case "$os" in
        darwin) os=macos ;;
        linux) os=linux ;;
        *) fail "unsupported OS: $os" ;;
    esac
    case "$arch" in
        x86_64) arch=amd64 ;;
        aarch64|arm64) arch=arm64 ;;
        *) fail "unsupported architecture: $arch" ;;
    esac
    printf 'usagi-%s-%s.tar.gz\n' "$os" "$arch"
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fail "sha256sum or shasum is required"
    fi
}

verify_checksum() {
    local checksum_file=$1 archive=$2 asset=$3 hash listed extra actual
    [ "$(wc -l < "$checksum_file" | tr -d ' ')" -eq 1 ] || fail "invalid checksum artifact for $asset"
    read -r hash listed extra < "$checksum_file" || fail "invalid checksum artifact for $asset"
    [ -z "${extra:-}" ] || fail "invalid checksum artifact for $asset"
    case "$hash" in
        *[!0-9a-fA-F]*|'') fail "invalid checksum artifact for $asset" ;;
    esac
    [ "${#hash}" -eq 64 ] || fail "invalid checksum artifact for $asset"
    [ "$listed" = "$asset" ] || fail "checksum artifact names unexpected asset: $listed"
    actual="$(sha256_of "$archive")"
    [ "$(printf '%s' "$actual" | tr '[:upper:]' '[:lower:]')" = "$(printf '%s' "$hash" | tr '[:upper:]' '[:lower:]')" ] || \
        fail "checksum mismatch for $asset"
}

verify_archive() {
    local archive=$1 entries details
    entries="$(tar -tzf "$archive")" || fail "could not read $archive"
    [ "$entries" = "usagi" ] || fail "archive must contain exactly one top-level usagi binary"
    details="$(tar -tvzf "$archive")" || fail "could not inspect $archive"
    case "$details" in
        -*) ;;
        *) fail "archive usagi entry must be a regular file" ;;
    esac
}

verify_expected_version() {
    local version_file=$1 candidate=$2 expected actual extra
    [ "$(wc -l < "$version_file" | tr -d ' ')" -eq 1 ] || fail "invalid version artifact"
    read -r expected extra < "$version_file" || fail "invalid version artifact"
    [ -n "$expected" ] && [ -z "${extra:-}" ] || fail "invalid version artifact"
    case "$expected" in
        v*) expected=${expected#v} ;;
    esac
    actual="$(read_version "$candidate")"
    [ -n "$actual" ] || fail "staged usagi did not report a valid version"
    [ "$actual" = "$expected" ] || fail "staged usagi version $actual does not match release $expected"
    printf '%s\n' "$actual"
}

acquire_lock

ASSET_NAME="$(platform_asset)"
BASE_URL="https://github.com/${REPO}/releases/latest/download"

mkdir -p -- "$BIN_DIR"
chmod 700 "$BIN_DIR"
STAGE_DIR="$(mktemp -d "$BIN_DIR/.update.XXXXXXXX")"
chmod 700 "$STAGE_DIR"

ARCHIVE="$STAGE_DIR/$ASSET_NAME"
CHECKSUM="$STAGE_DIR/$ASSET_NAME.sha256"
VERSION_FILE="$STAGE_DIR/$ASSET_NAME.version"

echo "Downloading and verifying $ASSET_NAME..."
curl -fsSL "$BASE_URL/$ASSET_NAME" -o "$ARCHIVE"
curl -fsSL "$BASE_URL/$ASSET_NAME.sha256" -o "$CHECKSUM"
curl -fsSL "$BASE_URL/$ASSET_NAME.version" -o "$VERSION_FILE"

verify_checksum "$CHECKSUM" "$ARCHIVE" "$ASSET_NAME"
verify_archive "$ARCHIVE"
tar -xzf "$ARCHIVE" -C "$STAGE_DIR" -- usagi
CANDIDATE="$STAGE_DIR/usagi"
chmod 755 "$CANDIDATE"
NEW_VERSION="$(verify_expected_version "$VERSION_FILE" "$CANDIDATE")"
OLD_VERSION="$(read_version "$TARGET")"

# STAGE_DIR is below BIN_DIR, so this rename stays on one filesystem. POSIX
# rename either replaces TARGET atomically or leaves its bytes and mode intact.
mv -f -- "$CANDIDATE" "$TARGET"

if [ -z "$OLD_VERSION" ]; then
    MESSAGE="usagi v${NEW_VERSION} をインストールしたよ！ぴょん"
    FACE="( ◕ω◕)"
elif [ "$OLD_VERSION" = "$NEW_VERSION" ]; then
    MESSAGE="usagi v${NEW_VERSION} は既に最新だよ！再インストールしたぴょん"
    FACE="( -ω-)"
else
    MESSAGE="usagi を v${OLD_VERSION} から v${NEW_VERSION} にぴょんしたよ！"
    OLD_MAJOR="${OLD_VERSION%%.*}"
    NEW_MAJOR="${NEW_VERSION%%.*}"
    OLD_MINOR="${OLD_VERSION#*.}"; OLD_MINOR="${OLD_MINOR%%.*}"
    NEW_MINOR="${NEW_VERSION#*.}"; NEW_MINOR="${NEW_MINOR%%.*}"
    if [ "$OLD_MAJOR" != "$NEW_MAJOR" ]; then
        FACE="(*^ω^)"
    elif [ "$OLD_MINOR" != "$NEW_MINOR" ]; then
        FACE="( ◕ω◕)"
    else
        FACE="( ^ω^)"
    fi
fi

C_RST=$'\033[0m'
C_BOLD=$'\033[1m'
C_PINK=$'\033[95m'
C_CYAN=$'\033[96m'
C_DIM=$'\033[2m'

printf "\n"
printf "   %s(\(\\%s\n" "$C_PINK" "$C_RST"
printf "   %s%s%s  %s%s%s\n" "$C_PINK" "$FACE" "$C_RST" "$C_BOLD" "$MESSAGE" "$C_RST"
printf '   %so_(")(")%s  %s→%s  %s%s/usagi%s\n' "$C_PINK" "$C_RST" "$C_DIM" "$C_RST" "$C_CYAN" "$BIN_DIR" "$C_RST"
printf "\n"

case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *)
        echo ""
        echo "Please add the following line to your shell configuration file (e.g., ~/.bashrc, ~/.zshrc):"
        echo "  export PATH=\"\$PATH:$BIN_DIR\""
        echo ""
        echo "After adding, restart your shell or run 'source <your-rc-file>' to apply the changes."
        ;;
esac
