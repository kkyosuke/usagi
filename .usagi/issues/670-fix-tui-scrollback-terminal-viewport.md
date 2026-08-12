---
number: 670
title: fix(tui): scrollback 上限後も terminal viewport の行を保持する
status: done
priority: high
labels: [review, v2, tui, terminal, bug]
dependson: []
related: [659, 660, 669]
created_at: 2026-08-12T22:53:33.327715+00:00
updated_at: 2026-08-12T23:18:03.038498+00:00
---

## レビュー基点

- reviewed commit: `b7e889da4cb1ff0dbc47b13c7de0150d169b0d11`
- 対象: v2 の live terminal viewport / VT scrollback / window projection
- 観点: 直近の「scroll 中は同じ retained row を保持する」契約が bounded history の境界でも成立するか

## Finding（P1 usability / long-running terminal）

`LiveTerminalControls::observe_rows` は、前 frame と現在の `total_rows` の差だけを「Agent が追記した行数」とみなし、scroll 中はその差を bottom offset へ足している。

しかし core VT parser の scrollback は 10,000 行で bounded であり、上限到達後は 1 行追記するたびに `append_scrollback` が oldest row を 1 行 evict する。このとき retained row count は不変なので、現行実装は追記を 0 行と判定する。結果として viewport は新しい出力のたびに 1 行ずつ前へ滑り、直前の修正が保証したはずの「読んでいる行を保持する」挙動が長時間 Agent で再発する。

同じ問題は daemon の per-terminal / aggregate cell budget、checkpoint frame budget が oldest history を trim する場合にも現れる。row count だけでは「append」「oldest eviction」「history replacement」を区別できない。

## 原因

viewport の座標が retained vector の index しか持たず、先頭から何行失われたかを示す monotonic origin を持たない。保持に必要な追記行数は次である。

```text
appended = max(0, current_origin + current_total - previous_origin - previous_total)
```

origin を含めず `current_total - previous_total` だけを見ると、append と eviction が相殺される。

## 対象責務

- core VT buffer は、oldest scrollback row を捨てるたびに buffer-local の monotonic retained-row origin を進める。
- semantic checkpoint は origin を運び、checkpoint size trimming でさらに row を落とす場合も payload の origin を進める。additive/default field として旧 payload を受理する。
- TUI projection は active buffer・`total_rows`・origin を観測し、scroll 中の append 数を論理末尾の差から算出する。checkpoint payload が前回より多い過去行を含んで origin が戻る場合も、論理末尾だけが増えた分を追記として扱う。buffer replacement は別座標系として混ぜず、現在の extent へ安全に clamp する。
- live bottom（scroll 0）、明示 ScrollBottom、focus ごとの view state、selection の snapshot contract は変えない。

## 受入条件

- [x] scrollback 上限到達後、append と oldest eviction が同時に起きて retained row count が不変でも、scroll 中 viewport は同じ content row を保持する。
- [x] oldest history の trim だけが起きた場合、surviving content の viewport は index を origin 差分だけ補正して保持する。
- [x] live bottom は従来どおり新しい出力へ追従し、`ScrollBottom` で 1 手で復帰する。
- [x] semantic checkpoint round-trip が retained-row origin を保持し、旧 checkpoint の欠落 field は origin 0 として安全に復元する。
- [x] checkpoint frame budget の history trimming が payload origin を落とした行数だけ進める。
- [x] `document/03-tui.md` の scrollback eviction 契約を実装と一致させる。
- [x] v2 の selected unit tests、fmt、check、clippy を通し、full test / coverage は PR CI で確認する。

## 検証

- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test -p usagi-core`（914 unit + 6 integration + 1 doc）
- `cargo test -p usagi-daemon`（759 unit + 35 integration）
- `cargo test -p usagi-tui`（1175 unit + 9 parity）
- `cargo test --workspace --quiet`
- `lychee --config lychee.toml --no-progress '*.md' 'document/**/*.md' 'v1/README.md' 'v1/document/**/*.md' '.agents/**/*.md' '.github/**/*.md'`
- `.usagi/issues/670-fix-tui-scrollback-terminal-viewport.md` の個別 link check

coverage 100% の最終判定は PR CI で行う。
