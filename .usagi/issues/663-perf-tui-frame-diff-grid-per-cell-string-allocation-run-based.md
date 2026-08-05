---
number: 663
title: perf(tui): frame diff grid の per-cell String allocation を run-based に削減する
status: todo
priority: medium
labels: [review, v2, tui, performance, rendering, memory]
dependson: []
related: [228, 554, 660]
parent: 654
created_at: 2026-08-05T13:51:06.531679+00:00
updated_at: 2026-08-05T13:51:06.531679+00:00
---

## Finding（P2 rendering / allocator pressure）

`presentation::frame::Frame` は端末セルごとに `Cell::Glyph { text: String, style: String }` を持つ。`Frame::from_lines` は各行を一度 `Vec<char>` 化し、可視glyphごとに `String` を作り、active styleもglyphごとにcloneする。FrameRendererはprevious/nextの2 frameを保持するため、この表現のresident memoryとallocation trafficを二重に持つ。

current buildのfocused allocation probe（120×40）では次の値だった。

| input | `Frame::from_lines` allocation |
|---|---:|
| plain 4,800 glyph | 4,921 alloc / 約342 KiB |
| 1つのSGR runを持つ4,800 glyph | 9,881 alloc / 約380 KiB |

これはterminal writeのspan数とは独立である。実際のfull diffは40 spanだけでも、grid生成はstyled glyphごとにstyle Stringをcloneする。pending/removal animationやmascotでdrawが続くとallocator trafficがframe rateに比例する。

## 修正方針

- frame cellはscalar/widthとstyle/run IDなどのcompact値を持ち、ANSI text/styleはframe-local internerまたはrow runで一度だけ所有する。
- line parserは`Vec<char>`全体を作らずstreaming iterator/indexでANSI/cursor/combining/wide glyphを処理する。
- diffはrow hash/run identityでunchanged rowを早くskipし、changed wide glyphの境界だけ展開する。最終Spanで初めてself-contained ANSI Stringを組み立てる。
- style equality、reset、combining marks、CJK width、cursor markerの現行observable contractは維持する。raw ANSI Stringをidentityとして複製するのではなく、frame内canonical styleをSSoTにする。

## 受入条件

- 120×40 plain/styled frameのallocation回数がglyph数に比例せず、row/style run数に比例することをallocation-counting test/benchで固定する。
- previous+next frameのresident payloadを測り、現行表現から明確に削減する。
- identical frameはcontent write 0、1-cell changeは必要spanのみ、wide glyph/combining/SGR/cursor/resize/reset parityを維持する。
- `Terminal::draw`は実IOだけを所有し、parse/diff最適化はpure frame moduleに閉じる。

## 根拠箇所

- `crates/tui/src/presentation/frame.rs`: `Cell`, `Frame::set_line`, `diff_spans`
- `src/runtime/tui.rs`: `CrosstermTerminal::draw`
