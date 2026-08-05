---
number: 662
title: fix(tui): scrollback 閲覧位置を新規出力から固定し page 移動・未読表示を追加する
status: todo
priority: medium
labels: [review, stability, tui, terminal, scroll, ux]
dependson: [655]
related: [344, 527, 637]
parent: 654
created_at: 2026-08-05T01:20:38.068535+00:00
updated_at: 2026-08-05T01:20:38.068535+00:00
---

## 症状

live terminalのscroll stateは「live bottomから何行上か」という`scroll: usize`だけを保持する。利用者が過去出力を読んでいる間に新しい行が追加されてもscroll値は変わらないため、viewportの絶対rowが新しい出力分だけ後ろへ動き、読んでいた内容が押し流される。

現行操作はmouse wheelまたは`Ctrl-O u/d`による1行移動だけで、長い履歴のpage移動、live bottomへの即時復帰、新着行数の表示がない。

## 既存issueとの境界

- #527はterminal pollingをUI threadから分離した。
- #637はscrollback全履歴projectionをviewport単位へ最適化した。
- #655はproduction key分類を統一し、plain PageUp/PageDown等をfocused PTYへ保持する。

本issueはview state/UXだけを扱い、pollingやVT parserを再実装しない。plain PageUp/PageDownをPTYから奪わず、TUI page scrollはCtrl-O prefix配下など明示予約された操作にする。

## 修正方針

- scrolled stateに前回total row countまたはstable retained-row anchorを保持する。
- `scroll > 0`中にN行追加された場合、履歴evictionが無い範囲ではoffsetをN増やして同じ絶対行をviewportに保つ。
- retention eviction/resize/checkpoint replaceではanchorを安全にclampし、誤ったrowやselectionへ飛ばさない。
- viewport高さ単位のolder/newer移動と、live bottomへ戻る明示actionを追加する。
- scrolled中はfooterへpaused状態と新着行数を短く表示する。

## 受け入れ条件

- 10行以上scroll upした状態で新しいoutputを1/N行追加しても、画面先頭・末尾の絶対retained rowが不変である。
- live bottomでは従来どおり新規outputへ追従する。
- page up/downが現在のviewport row数を基準に移動し、top/bottomでsaturateする。
- 明示actionで一度にlive bottomへ戻り、未読countをclearする。
- TUI page actionはCtrl-O prefix等の予約内だけで発火し、prefix無しPageUp/PageDown/Home/Endはfocused PTYへ従来bytesを送る。
- scroll中のselection、URL hit-test、CJK/wrapped line、terminal focus切替後のper-terminal state復元が正しい。
- history shrink/eviction、resize、checkpoint replaceでoffset/anchorをvalid rangeへ正規化する。
- fake screenにoutputを追加するpure testとreal PTYで連続出力中のviewport固定testを追加する。

## docs

`document/03-tui.md` のscrollback操作、paused/live-bottom、新着表示、prefix key表を更新する。
