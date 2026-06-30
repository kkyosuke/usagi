#!/bin/bash

set -e

# リポジトリ
REPO="KKyosuke/usagi"

# インストール先ディレクトリ
USAGI_DIR="$HOME/.usagi"
BIN_DIR="$USAGI_DIR/bin"

# バイナリからバージョン文字列を取り出す（取得できなければ空文字）
function read_version() {
    local bin="$1"
    [ -x "$bin" ] || return 0
    # `usagi --version` は "usagi 0.0.1" の形式。バージョン部分のみ取り出す
    "$bin" --version 2>/dev/null | awk '{print $NF}'
}

# OS/Arch 判別とダウンロード
function download_binary() {
    echo "Binary not found in current directory. Attempting to download..."

    OS="$(uname -s | tr '[:upper:]' '[:lower:]')"
    ARCH="$(uname -m)"

    case "$OS" in
      darwin) OS="macos" ;;
      linux) OS="linux" ;;
      *) echo "Unsupported OS: $OS"; exit 1 ;;
    esac

    case "$ARCH" in
      x86_64) ARCH="amd64" ;;
      aarch64|arm64) ARCH="arm64" ;;
      *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
    esac

    ASSET_NAME="usagi-${OS}-${ARCH}.tar.gz"
    URL="https://github.com/${REPO}/releases/latest/download/${ASSET_NAME}"

    echo "Downloading $ASSET_NAME from $URL..."
    # テンポラリディレクトリで展開
    TMP_DIR=$(mktemp -d)
    # 終了時に確実に削除
    trap 'rm -rf "$TMP_DIR"' EXIT

    # ダウンロードと展開
    curl -L "$URL" | tar -xz -C "$TMP_DIR"

    # 展開されたディレクトリからバイナリを見つける
    if [ -f "$TMP_DIR/usagi" ]; then
        cp "$TMP_DIR/usagi" .
    elif [ -f "$TMP_DIR/usagi.exe" ]; then
        cp "$TMP_DIR/usagi.exe" .
    else
        echo "Error: Binary not found in downloaded archive."
        exit 1
    fi
}

# バイナリがカレントディレクトリにない場合、ダウンロードを試みる
if [ ! -f "usagi" ] && [ ! -f "usagi.exe" ]; then
    download_binary
fi

# 既存インストールのバージョンを記録（アップデート判定用）
OLD_VERSION="$(read_version "$BIN_DIR/usagi")"

# ディレクトリ作成
echo "Creating directory $BIN_DIR..."
mkdir -p "$BIN_DIR"

# バイナリの移動 (カレントディレクトリにある想定)
BINARY_NAME="usagi"
if [ -f "usagi.exe" ]; then
    BINARY_NAME="usagi.exe"
fi

if [ -f "$BINARY_NAME" ]; then
    echo "Installing $BINARY_NAME to $BIN_DIR..."
    mv "$BINARY_NAME" "$BIN_DIR/"
else
    # 予備的なチェック（リネームされていない場合）
    SEARCHED_BIN=$(ls usagi usagi.exe 2>/dev/null | head -n 1)
    if [ -n "$SEARCHED_BIN" ]; then
        echo "Installing $SEARCHED_BIN as usagi to $BIN_DIR..."
        mv "$SEARCHED_BIN" "$BIN_DIR/usagi"
        BINARY_NAME="usagi"
    else
        echo "Error: usagi binary not found in current directory."
        exit 1
    fi
fi

# 権限変更
echo "Changing permissions for $BIN_DIR/$BINARY_NAME..."
chmod +x "$BIN_DIR/$BINARY_NAME"

# 新しくインストールされたバージョン
NEW_VERSION="$(read_version "$BIN_DIR/$BINARY_NAME")"

# インストール結果のメッセージと顔を決める
if [ -z "$OLD_VERSION" ]; then
    # 既存インストールなし → 新規
    MESSAGE="usagi v${NEW_VERSION} をインストールしたよ！ぴょん"
    FACE="( ◕ω◕)"
elif [ "$OLD_VERSION" = "$NEW_VERSION" ]; then
    # 同じバージョン → 再インストール
    MESSAGE="usagi v${NEW_VERSION} は既に最新だよ！再インストールしたぴょん"
    FACE="( -ω-)"
else
    # バージョンが変わった → アップデート（度合いで顔が変わる）
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
printf "   %so_(\")(\")%s  %s→%s  %s%s/%s%s\n" "$C_PINK" "$C_RST" "$C_DIM" "$C_RST" "$C_CYAN" "$BIN_DIR" "$BINARY_NAME" "$C_RST"
printf "\n"

# PATH 案内（まだ PATH に入っていない場合のみ）
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
