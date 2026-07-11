#!/usr/bin/env bash
set -eo pipefail

script=$(cd "$(dirname "$0")/../.." && pwd)/scripts/cargo-sccache.sh
tmp=$(mktemp -d "${TMPDIR:-/tmp}/usagi-cargo-sccache.XXXXXX")
tmp=$(cd "$tmp" && pwd)
trap 'rm -rf "$tmp"' EXIT

mkdir -p "$tmp/bin" "$tmp/repo/scripts"
cp "$script" "$tmp/repo/scripts/cargo-sccache.sh"
chmod +x "$tmp/repo/scripts/cargo-sccache.sh"
git -C "$tmp/repo" init -q
git -C "$tmp/repo" config user.email tests@example.com
git -C "$tmp/repo" config user.name Tests
git -C "$tmp/repo" commit --allow-empty -qm fixture
git_common_dir=$(git -C "$tmp/repo" rev-parse --path-format=absolute --git-common-dir)
expected_cache_dir=$(cd "$(dirname "$git_common_dir")" && pwd)/.usagi/cache/sccache

cat >"$tmp/bin/cargo" <<'EOF'
#!/usr/bin/env bash
{
  printf 'args=%s\n' "$*"
  printf 'RUSTC_WRAPPER=%s\n' "${RUSTC_WRAPPER-}"
  printf 'SCCACHE_DIR=%s\n' "${SCCACHE_DIR-}"
  printf 'SCCACHE_CACHE_SIZE=%s\n' "${SCCACHE_CACHE_SIZE-}"
} >"$OUT_FILE"
EOF
chmod +x "$tmp/bin/cargo"

(cd "$tmp/repo" && OUT_FILE="$tmp/no-sccache.txt" PATH="$tmp/bin:/usr/bin:/bin" scripts/cargo-sccache.sh test --quiet 2>"$tmp/no-sccache.err")
grep -q 'warning: sccache not found; running plain cargo' "$tmp/no-sccache.err"
grep -q '^args=test --quiet$' "$tmp/no-sccache.txt"
grep -q '^RUSTC_WRAPPER=$' "$tmp/no-sccache.txt"
grep -q '^SCCACHE_DIR=$' "$tmp/no-sccache.txt"

cat >"$tmp/bin/sccache" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
chmod +x "$tmp/bin/sccache"

(cd "$tmp/repo" && OUT_FILE="$tmp/with-sccache.txt" PATH="$tmp/bin:/usr/bin:/bin" scripts/cargo-sccache.sh clippy --all-targets)
grep -q '^args=clippy --all-targets$' "$tmp/with-sccache.txt"
grep -q '^RUSTC_WRAPPER=sccache$' "$tmp/with-sccache.txt"
grep -q "^SCCACHE_DIR=$expected_cache_dir$" "$tmp/with-sccache.txt"
grep -q '^SCCACHE_CACHE_SIZE=10G$' "$tmp/with-sccache.txt"

(cd "$tmp/repo" && OUT_FILE="$tmp/override.txt" PATH="$tmp/bin:/usr/bin:/bin" SCCACHE_DIR="$tmp/custom-cache" SCCACHE_CACHE_SIZE=5G RUSTC_WRAPPER=custom-wrapper scripts/cargo-sccache.sh fmt --all)
grep -q '^RUSTC_WRAPPER=custom-wrapper$' "$tmp/override.txt"
grep -q "^SCCACHE_DIR=$tmp/custom-cache$" "$tmp/override.txt"
grep -q '^SCCACHE_CACHE_SIZE=5G$' "$tmp/override.txt"

echo "cargo-sccache fixtures: ok"
