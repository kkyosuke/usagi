#!/usr/bin/env bash
set -euo pipefail

# scripts/coverage-report-comment.rb の fixture ベース検証。
# lcov.info を入力に、未達ファイル/関数/行・上限・エスケープ・全 100% を固定する。

root=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

gen() { GITHUB_WORKSPACE=/repo ruby "$root/scripts/coverage-report-comment.rb" "$@"; }

# --- ケース1: 未達関数と未達行が混在。関数 100% だが行未達のファイルも含む ---
cat > "$tmp/mixed.info" <<'LCOV'
SF:/repo/crates/tui/src/foo.rs
FN:10,foo::alpha
FN:20,foo::beta
FNDA:5,foo::alpha
FNDA:0,foo::beta
DA:10,5
DA:20,0
DA:21,0
end_of_record
SF:/repo/crates/core/src/bar.rs
FN:1,bar::only
FNDA:3,bar::only
DA:1,3
DA:2,0
end_of_record
SF:/repo/crates/cli/src/ok.rs
FN:1,ok::run
FNDA:1,ok::run
DA:1,1
end_of_record
LCOV

out=$(gen "$tmp/mixed.info")
# 未達関数は名前＋宣言行が出る
grep -Fqe '#### `crates/tui/src/foo.rs`' <<<"$out"
grep -Fqe '- `foo::beta` (L20)' <<<"$out"
# 関数不足量・行不足量
grep -Fqe '| `crates/tui/src/foo.rs` | 50.00% (不足 1) | 🔴 33.33% (不足 2) |' <<<"$out"
# 関数は全 hit だが行未達のファイル → 関数節なし・行節あり
grep -Fqe '| `crates/core/src/bar.rs` | 100.00% (不足 0) | 🔴 50.00% (不足 1) |' <<<"$out"
grep -Fqe '- 📈 未達行 (1): L2' <<<"$out"
# 100% のファイルは出さない
if grep -Fqe 'crates/cli/src/ok.rs' <<<"$out"; then
  echo "FAIL: fully-covered file must not appear" >&2
  exit 1
fi
# 未達 (<100%) は FAIL 判定
grep -Fqe '❌ FAIL' <<<"$out"

# --- ケース2: 関数・ファイル上限で切り詰め、超過分を明示 ---
{
  echo "SF:/repo/a.rs"
  for i in $(seq 1 5); do echo "FN:$i,f::g${i}"; done
  for i in $(seq 1 5); do echo "FNDA:0,f::g${i}"; done
  for i in $(seq 1 5); do echo "DA:$i,0"; done
  echo "end_of_record"
  echo "SF:/repo/b.rs"; echo "FN:1,b"; echo "FNDA:0,b"; echo "DA:1,0"; echo "end_of_record"
} > "$tmp/caps.info"
out=$(MAX_FILES=1 MAX_FUNCS_PER_FILE=3 gen "$tmp/caps.info")
grep -Fqe '  - …ほか 2 関数' <<<"$out"          # 5 件中 3 件表示
grep -Fqe '> …ほか 1 ファイル' <<<"$out"        # 2 件中 1 件表示
# 上限を超えたファイルは詳細節を出さない
if grep -Fqe '#### `b.rs`' <<<"$out"; then
  echo "FAIL: capped-out file must not have a detail section" >&2
  exit 1
fi

# --- ケース3: Markdown 敵対的な関数名 (パイプ) をエスケープ ---
cat > "$tmp/esc.info" <<'LCOV'
SF:/repo/p.rs
FN:6,f::pipe|name
FNDA:0,f::pipe|name
DA:6,0
end_of_record
LCOV
out=$(gen "$tmp/esc.info")
grep -Fqe '- `f::pipe\|name` (L6)' <<<"$out"

# --- ケース4: 全 100% → 祝いメッセージ・表なし ---
cat > "$tmp/perfect.info" <<'LCOV'
SF:/repo/a.rs
FN:1,a::x
FNDA:2,a::x
DA:1,2
end_of_record
LCOV
out=$(gen "$tmp/perfect.info")
grep -Fqe '✅ PASS' <<<"$out"
grep -Fqe 'パーフェクト' <<<"$out"
if grep -Fqe '| 📄 ファイル |' <<<"$out"; then
  echo "FAIL: perfect coverage must not render a table" >&2
  exit 1
fi

# --- ケース5: 未達行レンジの上限 ---
{
  echo "SF:/repo/r.rs"
  echo "FN:1,r::f"
  echo "FNDA:1,r::f"
  echo "DA:1,1"
  # 連続しない未達行を多数 (奇数行) 作り、レンジが上限を超える
  for i in 3 5 7 9 11; do echo "DA:$i,0"; done
  echo "end_of_record"
} > "$tmp/ranges.info"
out=$(MAX_LINE_RANGES=2 gen "$tmp/ranges.info")
grep -Fqe '…ほか 3 箇所' <<<"$out"

# --- ケース6: 入力ファイル欠如は非ゼロ終了 ---
if gen "$tmp/does-not-exist.info" >/dev/null 2>&1; then
  echo "FAIL: missing lcov must exit non-zero" >&2
  exit 1
fi

echo "coverage-report-comment: ok"
