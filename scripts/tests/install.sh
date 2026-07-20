#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
INSTALLER="$ROOT/scripts/install.sh"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "$TEST_ROOT"' EXIT

case "$(uname -s)" in
    Darwin) TEST_OS=macos ;;
    Linux) TEST_OS=linux ;;
    *) echo "unsupported test OS" >&2; exit 1 ;;
esac
case "$(uname -m)" in
    x86_64) TEST_ARCH=amd64 ;;
    arm64|aarch64) TEST_ARCH=arm64 ;;
    *) echo "unsupported test architecture" >&2; exit 1 ;;
esac
ASSET="usagi-$TEST_OS-$TEST_ARCH.tar.gz"

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        shasum -a 256 "$1" | awk '{print $1}'
    fi
}

mode() {
    if stat -f '%Lp' "$1" >/dev/null 2>&1; then
        stat -f '%Lp' "$1"
    else
        stat -c '%a' "$1"
    fi
}

make_binary() {
    local path=$1 version=$2 marker=$3
    printf '#!/bin/sh\nprintf "usagi %s\\n"\n# %s\n' "$version" "$marker" > "$path"
    chmod 755 "$path"
}

make_fixture() {
    local dir=$1 version=${2:-2.0.0}
    mkdir -p "$dir/content"
    make_binary "$dir/content/usagi" "$version" candidate
    tar -czf "$dir/$ASSET" -C "$dir/content" usagi
    printf '%s  %s\n' "$(sha256 "$dir/$ASSET")" "$ASSET" > "$dir/$ASSET.sha256"
    printf 'v%s\n' "$version" > "$dir/$ASSET.version"
}

make_fake_curl() {
    local bin_dir=$1
    mkdir -p "$bin_dir"
    cat > "$bin_dir/curl" <<'SH'
#!/bin/sh
set -eu
output=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) output=$2; shift 2 ;;
        -*) shift ;;
        *) url=$1; shift ;;
    esac
done
[ -n "$output" ] && [ -n "$url" ]
[ "$(LC_ALL=C ls -ld "$(dirname "$output")" | cut -c1-10)" = "drwx------" ] || exit 71
if [ -n "${FAKE_CURL_GUARD:-}" ]; then
    mkdir "$FAKE_CURL_GUARD" || exit 70
    trap 'rmdir "$FAKE_CURL_GUARD"' EXIT
    sleep 0.05
fi
cp "$FIXTURE_DIR/${url##*/}" "$output"
SH
    chmod 755 "$bin_dir/curl"
}

prepare_case() {
    CASE_DIR="$TEST_ROOT/$1"
    FIXTURE_DIR="$CASE_DIR/fixture"
    HOME_DIR="$CASE_DIR/home"
    CWD_DIR="$CASE_DIR/cwd"
    FAKE_BIN="$CASE_DIR/fake-bin"
    mkdir -p "$FIXTURE_DIR" "$HOME_DIR/.usagi/bin" "$CWD_DIR"
    make_fixture "$FIXTURE_DIR"
    make_fake_curl "$FAKE_BIN"
    make_binary "$HOME_DIR/.usagi/bin/usagi" 1.0.0 installed-old
    chmod 751 "$HOME_DIR/.usagi/bin/usagi"
    cp "$HOME_DIR/.usagi/bin/usagi" "$CASE_DIR/old-bytes"
}

run_installer() {
    (cd "$CWD_DIR" && HOME="$HOME_DIR" FIXTURE_DIR="$FIXTURE_DIR" \
        PATH="$FAKE_BIN:$PATH" bash "$INSTALLER")
}

assert_old_preserved() {
    cmp "$CASE_DIR/old-bytes" "$HOME_DIR/.usagi/bin/usagi"
    [ "$(mode "$HOME_DIR/.usagi/bin/usagi")" = 751 ]
}

expect_failure() {
    if run_installer >"$CASE_DIR/out" 2>"$CASE_DIR/err"; then
        echo "expected installer failure for $CASE_DIR" >&2
        exit 1
    fi
    assert_old_preserved
}

prepare_case success
make_binary "$CWD_DIR/usagi" 99.0.0 malicious-cwd-sentinel
cp "$CWD_DIR/usagi" "$CASE_DIR/sentinel"
run_installer >/dev/null
[ "$(readlink "$HOME_DIR/.usagi/bin/usagi" 2>/dev/null || true)" = "" ]
[ "$("$HOME_DIR/.usagi/bin/usagi" --version)" = "usagi 2.0.0" ]
[ "$(mode "$HOME_DIR/.usagi/bin/usagi")" = 755 ]
cmp "$CASE_DIR/sentinel" "$CWD_DIR/usagi"
[ -z "$(find "$HOME_DIR/.usagi/bin" -maxdepth 1 -name '.update.*' -print)" ]

prepare_case bad-checksum
printf '%064d  %s\n' 0 "$ASSET" > "$FIXTURE_DIR/$ASSET.sha256"
expect_failure

prepare_case truncated
head -c 20 "$FIXTURE_DIR/$ASSET" > "$FIXTURE_DIR/truncated"
mv "$FIXTURE_DIR/truncated" "$FIXTURE_DIR/$ASSET"
printf '%s  %s\n' "$(sha256 "$FIXTURE_DIR/$ASSET")" "$ASSET" > "$FIXTURE_DIR/$ASSET.sha256"
expect_failure

prepare_case unexpected-entry
printf 'extra\n' > "$FIXTURE_DIR/content/extra"
tar -czf "$FIXTURE_DIR/$ASSET" -C "$FIXTURE_DIR/content" usagi extra
printf '%s  %s\n' "$(sha256 "$FIXTURE_DIR/$ASSET")" "$ASSET" > "$FIXTURE_DIR/$ASSET.sha256"
expect_failure

prepare_case symlink-entry
rm "$FIXTURE_DIR/content/usagi"
ln -s /tmp/not-usagi "$FIXTURE_DIR/content/usagi"
tar -czf "$FIXTURE_DIR/$ASSET" -C "$FIXTURE_DIR/content" usagi
printf '%s  %s\n' "$(sha256 "$FIXTURE_DIR/$ASSET")" "$ASSET" > "$FIXTURE_DIR/$ASSET.sha256"
expect_failure

prepare_case wrong-version
printf 'v2.0.1\n' > "$FIXTURE_DIR/$ASSET.version"
expect_failure

prepare_case missing-verification-artifact
rm "$FIXTURE_DIR/$ASSET.version"
expect_failure

prepare_case rename-failure
cat > "$FAKE_BIN/mv" <<'SH'
#!/bin/sh
exit 73
SH
chmod 755 "$FAKE_BIN/mv"
expect_failure

prepare_case concurrent
FAKE_CURL_GUARD="$CASE_DIR/curl-active"
export FAKE_CURL_GUARD
run_installer >"$CASE_DIR/first.out" 2>"$CASE_DIR/first.err" &
first_pid=$!
run_installer >"$CASE_DIR/second.out" 2>"$CASE_DIR/second.err" &
second_pid=$!
wait "$first_pid"
wait "$second_pid"
unset FAKE_CURL_GUARD
[ "$("$HOME_DIR/.usagi/bin/usagi" --version)" = "usagi 2.0.0" ]

echo "install.sh regression tests passed"
