---
number: 199
title: perf(daemon): vt100 scrollback の権威を daemon に一元化する
status: todo
priority: medium
labels: [perf, daemon, tui, terminal, review]
dependson: []
related: [167, 172, 174]
parent: 159
created_at: 2026-07-11T01:30:37.458832+00:00
updated_at: 2026-07-11T02:45:02Z
---

## 背景

remote paneはdaemon側の `PtySession` と各TUI client側の `DaemonTerminal` が、それぞれ同じscrollback上限のvt100 parserを保持する。daemonはattach/resync用screenを構築し、clientはraw PTY outputを再生してもう一つのgridを構築する。

`#172` はended paneのgrid解放、`#174` はscrollback行圧縮を実装したが、どちらのissue本文もdaemon移行後の恒久解を「grid権威の一元化」としている。複数TUI clientが同じterminalへattachするとclient parser分がさらに増える。

## 方針

- daemonをterminal grid / scrollbackの唯一の権威とする。
- clientへは可視viewport snapshotと差分、cursor/attrs等の描画に必要な情報だけを送る。
- client側はfull scrollback parserを持たず、表示領域と選択/hoverに必要なbounded stateだけを保持する。
- background/non-visible paneのsubscriptionを必要に応じて解除し、再表示時にsnapshotで復帰する。
- raw output protocolとの互換・version negotiationを定義する。
- URL/PR harvestをdaemon側または一元化されたhistory APIへ移す。

## 受け入れ条件

- 1 terminalのscrollback本文をdaemon/TUIで重複保持しない。
- client数を増やしてもscrollback履歴サイズ分のRSSが線形増加しない。
- attach/resync、resize/reflow、selection/copy、URL hover、alt-screenの表示が不変。
- slow clientとbacklog eviction後も正しい画面へ復帰する。

## テスト・計測

- 1/10/100 pane、1/2/5 clientのdaemon/TUI RSSを分離計測。
- 長いscrollback、wide char、SGR、alt-screen、resize/reflowのprotocol test。
- slow clientのresync E2E。
- #172/#174のmemory回帰計測をdaemon構成で継続する。
