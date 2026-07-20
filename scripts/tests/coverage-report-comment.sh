#!/usr/bin/env bash
set -euo pipefail

# scripts/coverage-report-comment.rb の fixture ベース検証。
# 入力は cargo-llvm-cov の JSON レポート。未達ファイル/関数(マージ済み)/行・上限・
# エスケープ・demangle・全 100% を固定する。region 形式は
# [lineStart, colStart, lineEnd, colEnd, execCount, fileID, expandedFileID, kind]。

root=$(cd "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

gen() { GITHUB_WORKSPACE=/repo ruby "$root/scripts/coverage-report-comment.rb" "$@"; }

# --- ケース1: 未達関数(マージ済み)と未達行。関数 100% だが行未達のファイルも含む ---
cat > "$tmp/mixed.json" <<'JSON'
{"data":[{
  "files":[
    {"filename":"/repo/crates/tui/src/foo.rs","summary":{"functions":{"count":2,"covered":1,"percent":50},"lines":{"count":4,"covered":1,"percent":25}}},
    {"filename":"/repo/crates/core/src/bar.rs","summary":{"functions":{"count":1,"covered":1,"percent":100},"lines":{"count":2,"covered":1,"percent":50}}},
    {"filename":"/repo/crates/cli/src/ok.rs","summary":{"functions":{"count":1,"covered":1,"percent":100},"lines":{"count":1,"covered":1,"percent":100}}}
  ],
  "functions":[
    {"name":"foo::alpha","count":5,"filenames":["/repo/crates/tui/src/foo.rs"],"regions":[[10,1,10,20,5,0,0,0]]},
    {"name":"foo::beta","count":0,"filenames":["/repo/crates/tui/src/foo.rs"],"regions":[[20,1,22,5,0,0,0,0]]},
    {"name":"bar::only","count":3,"filenames":["/repo/crates/core/src/bar.rs"],"regions":[[1,1,1,30,3,0,0,0],[2,1,2,10,0,0,0,0]]},
    {"name":"ok::run","count":1,"filenames":["/repo/crates/cli/src/ok.rs"],"regions":[[1,1,1,10,1,0,0,0]]}
  ],
  "totals":{"functions":{"count":4,"covered":3,"percent":75},"lines":{"count":7,"covered":3,"percent":42}}
}]}
JSON

out=$(gen "$tmp/mixed.json")
# 未達関数はマージ後の名前(demangle 済み)＋宣言行が出る
grep -Fqe '#### `crates/tui/src/foo.rs`' <<<"$out"
grep -Fqe '- `foo::beta` (L20)' <<<"$out"
# 関数不足量(マージ済み)・行不足量は JSON summary と一致
grep -Fqe '| `crates/tui/src/foo.rs` | 50.00% (不足 1) | 🔴 25.00% (不足 3) |' <<<"$out"
grep -Fqe '- 📈 未達行 (3): L20-22' <<<"$out"
# 関数は全 hit だが行未達のファイル → 関数節なし・行節あり
grep -Fqe '| `crates/core/src/bar.rs` | 100.00% (不足 0) | 🔴 50.00% (不足 1) |' <<<"$out"
grep -Fqe '- 📈 未達行 (1): L2' <<<"$out"
# 100% のファイルは出さない
if grep -Fqe 'crates/cli/src/ok.rs' <<<"$out"; then
  echo "FAIL: fully-covered file must not appear" >&2
  exit 1
fi
grep -Fqe '❌ FAIL' <<<"$out"

# --- ケース2: 関数・ファイル上限で切り詰め、超過分を明示 ---
cat > "$tmp/caps.json" <<'JSON'
{"data":[{
  "files":[
    {"filename":"/repo/a.rs","summary":{"functions":{"count":5,"covered":0,"percent":0},"lines":{"count":5,"covered":0,"percent":0}}},
    {"filename":"/repo/b.rs","summary":{"functions":{"count":1,"covered":0,"percent":0},"lines":{"count":1,"covered":0,"percent":0}}}
  ],
  "functions":[
    {"name":"a::g1","count":0,"filenames":["/repo/a.rs"],"regions":[[1,1,1,9,0,0,0,0]]},
    {"name":"a::g2","count":0,"filenames":["/repo/a.rs"],"regions":[[2,1,2,9,0,0,0,0]]},
    {"name":"a::g3","count":0,"filenames":["/repo/a.rs"],"regions":[[3,1,3,9,0,0,0,0]]},
    {"name":"a::g4","count":0,"filenames":["/repo/a.rs"],"regions":[[4,1,4,9,0,0,0,0]]},
    {"name":"a::g5","count":0,"filenames":["/repo/a.rs"],"regions":[[5,1,5,9,0,0,0,0]]},
    {"name":"b::only","count":0,"filenames":["/repo/b.rs"],"regions":[[1,1,1,9,0,0,0,0]]}
  ],
  "totals":{"functions":{"count":6,"covered":0,"percent":0},"lines":{"count":6,"covered":0,"percent":0}}
}]}
JSON
out=$(MAX_FILES=1 MAX_FUNCS_PER_FILE=3 gen "$tmp/caps.json")
grep -Fqe '  - …ほか 2 関数' <<<"$out"          # 5 件中 3 件表示
grep -Fqe '> …ほか 1 ファイル' <<<"$out"        # 2 件中 1 件表示
if grep -Fqe '#### `b.rs`' <<<"$out"; then
  echo "FAIL: capped-out file must not have a detail section" >&2
  exit 1
fi

# --- ケース3: Markdown 敵対的な関数名 (パイプ) をエスケープ ---
cat > "$tmp/esc.json" <<'JSON'
{"data":[{
  "files":[{"filename":"/repo/p.rs","summary":{"functions":{"count":1,"covered":0,"percent":0},"lines":{"count":1,"covered":0,"percent":0}}}],
  "functions":[{"name":"f::pipe|name","count":0,"filenames":["/repo/p.rs"],"regions":[[6,1,6,10,0,0,0,0]]}],
  "totals":{"functions":{"count":1,"covered":0,"percent":0},"lines":{"count":1,"covered":0,"percent":0}}
}]}
JSON
out=$(gen "$tmp/esc.json")
grep -Fqe '- `f::pipe\|name` (L6)' <<<"$out"

# --- ケース4: 全 100% → 祝いメッセージ・表なし ---
cat > "$tmp/perfect.json" <<'JSON'
{"data":[{
  "files":[{"filename":"/repo/a.rs","summary":{"functions":{"count":1,"covered":1,"percent":100},"lines":{"count":1,"covered":1,"percent":100}}}],
  "functions":[{"name":"a::x","count":2,"filenames":["/repo/a.rs"],"regions":[[1,1,1,10,2,0,0,0]]}],
  "totals":{"functions":{"count":1,"covered":1,"percent":100},"lines":{"count":1,"covered":1,"percent":100}}
}]}
JSON
out=$(gen "$tmp/perfect.json")
grep -Fqe '✅ PASS' <<<"$out"
grep -Fqe 'パーフェクト' <<<"$out"
if grep -Fqe '| 📄 ファイル |' <<<"$out"; then
  echo "FAIL: perfect coverage must not render a table" >&2
  exit 1
fi

# --- ケース5: 未達行レンジの上限（関数は covered、行のみ未達）---
cat > "$tmp/ranges.json" <<'JSON'
{"data":[{
  "files":[{"filename":"/repo/r.rs","summary":{"functions":{"count":1,"covered":1,"percent":100},"lines":{"count":6,"covered":1,"percent":16}}}],
  "functions":[{"name":"r::f","count":1,"filenames":["/repo/r.rs"],"regions":[[1,1,1,10,1,0,0,0],[3,1,3,5,0,0,0,0],[5,1,5,5,0,0,0,0],[7,1,7,5,0,0,0,0],[9,1,9,5,0,0,0,0],[11,1,11,5,0,0,0,0]]}],
  "totals":{"functions":{"count":1,"covered":1,"percent":100},"lines":{"count":6,"covered":1,"percent":16}}
}]}
JSON
out=$(MAX_LINE_RANGES=2 gen "$tmp/ranges.json")
grep -Fqe '…ほか 3 箇所' <<<"$out"
if grep -Fqe '未達関数' <<<"$out"; then
  echo "FAIL: fully-covered functions must not list 未達関数" >&2
  exit 1
fi

# --- ケース6: gap region (kind=2) は未達行に数えない ---
cat > "$tmp/gap.json" <<'JSON'
{"data":[{
  "files":[{"filename":"/repo/g.rs","summary":{"functions":{"count":1,"covered":1,"percent":100},"lines":{"count":2,"covered":1,"percent":50}}}],
  "functions":[{"name":"g::f","count":1,"filenames":["/repo/g.rs"],"regions":[[1,1,1,10,1,0,0,0],[2,1,2,5,0,0,0,0],[3,1,3,5,0,0,0,2]]}],
  "totals":{"functions":{"count":1,"covered":1,"percent":100},"lines":{"count":2,"covered":1,"percent":50}}
}]}
JSON
out=$(gen "$tmp/gap.json")
grep -Fqe '- 📈 未達行 (1): L2' <<<"$out"     # L3 は gap なので出ない
if grep -Fqe 'L3' <<<"$out"; then
  echo "FAIL: gap region line must not be reported" >&2
  exit 1
fi

# --- ケース7: Rust v0 mangled 名の demangle（c++filt がある環境のみ）---
cat > "$tmp/mangle.json" <<'JSON'
{"data":[{
  "files":[{"filename":"/repo/d.rs","summary":{"functions":{"count":1,"covered":0,"percent":0},"lines":{"count":1,"covered":0,"percent":0}}}],
  "functions":[{"name":"_RNvMs5_NtNtCs5fR1l2oH6y7_10usagi_core6domain5agentNtB5_21DurableLaunchSnapshot3new","count":0,"filenames":["/repo/d.rs"],"regions":[[1,1,1,10,0,0,0,0]]}],
  "totals":{"functions":{"count":1,"covered":0,"percent":0},"lines":{"count":1,"covered":0,"percent":0}}
}]}
JSON
if command -v c++filt >/dev/null 2>&1; then
  out=$(gen "$tmp/mangle.json")
  grep -Fqe 'DurableLaunchSnapshot' <<<"$out"
  if grep -Fqe '_RNvMs5_' <<<"$out"; then
    echo "FAIL: mangled name must be demangled when c++filt is present" >&2
    exit 1
  fi
fi
# DEMANGLER 無効化時は素の名前のまま
out=$(DEMANGLER= gen "$tmp/mangle.json")
grep -Fqe '_RNvMs5_' <<<"$out"

# --- ケース8: 入力ファイル欠如は非ゼロ終了 ---
if gen "$tmp/does-not-exist.json" >/dev/null 2>&1; then
  echo "FAIL: missing json must exit non-zero" >&2
  exit 1
fi

echo "coverage-report-comment: ok"
