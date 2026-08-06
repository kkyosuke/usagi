---
number: 667
title: fix(tui): scrollback viewport を新規出力から固定し live 復帰を表示する
status: todo
priority: medium
labels: [review, v2, tui, terminal, uiux, scroll, usability]
dependson: []
related: [304, 597, 637, 659]
parent: 664
created_at: 2026-08-06T20:39:26.170496+00:00
updated_at: 2026-08-06T22:16:15.188518+00:00
---

## Finding（P2 scroll UX / visibility）

`LiveTerminalControls` はscroll位置を「live bottomから何行上か」の `scroll: usize` だけで保持する。`visible_range` は毎回 `end = total_rows - scroll` とするため、`scroll > 0`で過去出力を読んでいてもnew outputで`total_rows`が増えるとviewportが同じ行数だけ前へ流れる。

例: total=100 / viewport=10 / scroll=5 は85..95を表示するが、3行追加後はscrollが5のまま88..98になる。ユーザーが読んでいた行を保持できない。

またfooterは`[Closeup] active pane`等だけで、live bottomを離れていること、新着行数、復帰操作を示さない。scroll中もPTY inputは受理されるため、echo/応答が画面外に増えても利用者が気づきにくい。

## 修正方針

- terminal-local view stateを `FollowingLive` / `Anchored` の明示modeにする。scroll=0はlive-follow、最初のscroll upで現在viewportのretained logical row intervalをanchorする。
- output追加時はanchored viewportを同じ論理行へ保つ。実装はappend/eviction deltaまたはstable retained-row originを `TerminalSession` / `TerminalScreen` から供給し、単に毎frameのbottom-relative値を再利用しない。
- oldest retention evictionでanchor先が失われた場合は、最古のavailable rowへclampし `history truncated` を安全に表示する。snapshot/resyncでrow identityを証明できない場合も明示的にclampし、別内容を同じ位置として見せない。
- anchored中の新規出力件数をterminal identityごとに集計し、footerへ例 `↓ 17 new · End: live` を表示する。0へ戻る操作でcountをclearする。
- wheel down / `Ctrl-O d`は従来どおり1行live方向へ進み、0到達でFollowingLiveへ戻す。`End`または明示chordで一度にlive bottomへ戻る。`PageUp` / `PageDown`はviewport単位で移動できるようにする。
- focus切替後もmode/anchor/new-countをbounded cacheで復元する。selection / pointer hit-testは固定viewportのrow originと一致させる。
- new countは受信chunk数やraw改行数でなくretained logical row originの増分から求め、CR上書き・cursor移動・alternate screen更新・resize reflowで水増ししない。`usize`/表示幅を超える場合はsaturating countと省略表示にする。
- follow modeでhistoryが縮む/resyncされる場合は常に新しいlive bottomへ追従し、anchored modeだけがclamp/truncation feedbackを持つ。0行・viewportより短いhistory・height 0/1でもmodeが反転しない。

## 受入条件

- scroll up後にN行outputが追加されても、evictionが無ければ画面の先頭/末尾logical rowと内容が変わらない。
- new countが正しく増え、live復帰で0になる。scroll中のinput echoもnew countへ含まれる。
- retention eviction、history shrink、resize、checkpoint resync、alternate screen切替でstale rowを同じanchorとして表示しない。
- tab/focus切替で各terminalのscroll mode、anchor、new countを独立保持する。
- footerが狭幅/CJKでもclipし、terminal content rowsとpointer geometryをずらさない。
- `End`/live復帰はretained selectionを意図せず消さず、selection snapshotがevictionで表示不能になった場合だけtyped feedbackとともに安全にclear/clampする。

## 必須テスト

- total rows増加、cap eviction、resync shrink、focus切替をfake revision/originで固定する。
- continuous output中のwheel/PageUp/PageDown/End、CR overwrite、alternate screen、resize reflowとnew countを検証する。
- selection/clickのrow mappingがanchored viewportと一致することを確認する。
- 0/1-row viewport、`usize`上限近傍のorigin/count、cache eviction後の再focusを固定する。

## 根拠箇所

- `crates/tui/src/presentation/live_terminal.rs`: `TerminalViewState::scroll`, `visible_range`
- `crates/tui/src/presentation/mod.rs`: terminal material key / pointer mapping
- `crates/tui/src/presentation/widgets/live_terminal.rs`: footer / viewport
- `crates/tui/src/usecase/application/terminal_screen.rs`: retained row indexing
- `document/03-tui.md`: live terminal scroll contract
