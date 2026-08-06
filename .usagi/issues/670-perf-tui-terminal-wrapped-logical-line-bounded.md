---
number: 670
title: perf(tui): terminal の長い wrapped logical line 投影を bounded にする
status: todo
priority: medium
labels: [review, v2, tui, terminal, uiux, performance, rendering, links]
dependson: []
related: [389, 637, 666, 668]
parent: 664
created_at: 2026-08-06T22:16:15.188518+00:00
updated_at: 2026-08-06T22:16:15.188518+00:00
---

## Finding（P2 display latency / pathological wrapped line）

#637 は通常のretained history投影をviewport行数へ縮退させたが、URL underlineのcorrectnessのため、viewportへ接する**logical line全体**を同期scanする。

- `TerminalScreen::rows_with_scrollback_window` / `_selection` は `logical_scan_range` でwindow先頭から、直前row末尾がnon-blankである間 `scan_start -= 1` を繰り返す。
- 同様にwindow末尾からwrapped successorを走査する。
- その範囲をANSI-free `Vec<String>`へ変換し、`terminal_link::scan_links`がdisplay-column gridへ再展開する。

通常の改行主体historyではviewport分だけで済む一方、terminal幅いっぱいのrowを改行なしで10,000行分出すと、全rowが1本のwrapped logical lineになる。live bottomの24行だけを描画していても、毎回最大10,000行まで逆走・文字列化・link scanし得る。#637の「通常経路はviewport比例」は成立するが、pathological outputに対するhard boundが無く、#666でVT applyをtime-sliceしてもその後の1 projectionがinput / scroll / quitをblockし得る。

## 修正方針

- retained row metadataにlogical-line start/endまたはbounded link materialを持ち、viewport投影のたびにwrapped chainを線形探索しない。metadata更新はappend/overwrite/resize/eviction/checkpoint replaceのrevisionへfenceする。
- URL検出のためにlogical line全体が必要でも、scan対象byte/rowへhard boundを置く。boundを超えるlogical lineは「リンク装飾なしの通常text」として安全に描画し、clickもopenしない。表示内容そのものは省略・dropしない。
- incremental/background化する場合、1 frameのrow/byte/time budgetで分割し、未完成link materialをsuccessとしてpublishしない。screen revision、retained origin、geometryでfenceし、古いscan結果が新しいviewportを上書きしない。
- CJK/wide glyph、continuation cell、alternate screen、resize reflow、oldest evictionでlogical-line boundaryを誤認しない。metadata/memoryはscrollback capとterminal幅に対してhard boundを持つ。
- #668のclick/drag viewport snapshotは同じbounded link materialを再利用し、表示ではリンク無しなのにclickだけ全履歴scanして開く、またはその逆を作らない。

## 受入条件

- 80-columnを10,000 physical rows以上連続して埋める改行なしoutputでも、viewport projection 1回のvisited rows/bytes/timeが設定boundを超えない。
- bound内の1〜複数row wrapped URLは従来どおり全cellがunderlineされ、どのcellのclickでも同じURLを開く。
- bound超過logical lineは全textを通常表示するが、部分URLを誤ってunderline/openせず、typed metric/diagnosticでdegradeを観測できる。
- append、backspace/CR overwrite、cursor move、resize、eviction、checkpoint resync、focus switch後もstale link cell・wrong URL openがない。
- continuous pathological output中もkey / scroll / modal / quitが規定frame数以内に処理され、projection workとcache memoryがhard bound内に留まる。

## 必須テスト・計測

- visited-row/byte counterで、10,000-row wrapped chainの末尾24-row windowが全historyをscanしないことをassertする。
- bound直前/一致/直後のwrapped URL、URLがboundを跨ぐ場合、CJK/wide glyph、blank terminal row、alternate screenを固定する。
- output/revision/resize/evictionをscan途中へ挟み、stale materialのdropと再計算を検証する。
- release benchmarkを100/1,000/10,000 wrapped rowsで取り、projection latencyがhistory長に線形増加しないことを記録する。

## 根拠箇所

- `crates/tui/src/usecase/application/terminal_screen.rs`: `logical_scan_range`, `rows_with_scrollback_window`, `rows_with_scrollback_window_selection`
- `crates/tui/src/usecase/application/terminal_link.rs`: `scan_links`, display-column expansion
- `crates/tui/src/presentation/mod.rs`: `controller_terminal_view`, terminal material cache
- `crates/core/src/usecase/vt_screen.rs`: retained row / wrap cell authority
