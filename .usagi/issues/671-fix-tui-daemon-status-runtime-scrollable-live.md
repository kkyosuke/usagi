---
number: 671
title: fix(tui): daemon status の runtime 一覧を scrollable / live にする
status: todo
priority: medium
labels: [review, v2, tui, daemon, uiux, status, modal, observability]
dependson: []
related: [374, 551, 643, 644, 645, 658]
parent: 664
created_at: 2026-08-06T22:20:12.306400+00:00
updated_at: 2026-08-06T22:20:12.306400+00:00
---

## Finding（P2 daemon visibility / stale display）

Overview の `daemon` status modal は表示域を超えた Agent runtime を `↓ N more` と畳むが、その隠れた行へ移動する操作を持たない。

- `daemon_modal::runtime_lines` は常にinventory先頭から `capacity - 1` 行だけを描き、残りを `↓ N more` にする。offset / selection / anchorを受け取らない。
- controllerの `Overlay::Daemon` は `Escape` だけを処理し、`Up` / `Down` / `PageUp` / `PageDown` / `Home` / `End`をすべて無視する。
- wheelはlive-terminal用 `ScrollUp` / `ScrollDown`へ分類され、foreground overlay中はbackground paneを守るため先にconsumeされるのでmodalへ届かない。

したがって小さいterminalやruntime数が多いworkspaceでは、画面自身が「more」と示す情報を利用者が読む手段がない。

表示鮮度にも別の問題がある。modalを開くと `refresh_agent_inventory` がcacheを空にしてcoalesced restore laneへ**1回だけ**観測を要求するが、成功後の `RestoreRetryState` は次回を予定しない。metricsとsession projectionは更新されても、modalを開いたまま起きたAgentのreserved→live、exit、interrupted、reclaimed等はruntime一覧へ反映されない。再接続や別のlocal mutationが無ければ、古い一覧が鮮度表示なしで現在値に見える。

## 修正方針

- daemon status専用のview stateとしてruntime viewport offsetまたはexact `AgentRuntimeId` anchorを持つ。`Up` / `Down`は1行、`PageUp` / `PageDown`はviewport単位、`Home` / `End`は先頭/末尾へ移動し、wheelもmodalがfrontmostの間だけ同じscrollへrouteする。
- 描画は共通 `modal::viewport_window` / `scroll_window`（または同等のbounded helper）を使い、`↑ N more` / `↓ N more`とfooterの操作hintを実挙動に一致させる。0/1行capacity、狭幅/CJKでもmodal geometryを溢れさせない。
- inventory更新で先頭へ行がinsert/removeされても、可能な限りexact runtime identityをanchorして同じ行を保持する。anchorが消えた場合だけ最寄りavailable rowへclampする。短縮runtime IDやsession labelをidentityに使わない。
- modalを開いている間はdisplay-only inventory laneをbounded cadence（例500ms〜1s）、one in-flight、coalesced wakeで更新する。frame loopはenqueue/drainだけを行い、daemon RPCを同期実行しない。既存restore laneを再利用する場合も、表示refreshがpane restoreやdurable intent mutationを不要に繰り返さない責務分離を保つ。
- refresh中は直前のvalid snapshotを保持し、初回だけloadingを出す。各snapshotへclient observation time/generationを持たせ、refresh failure / staleは明示する。失敗を空inventoryやhealthyとして表示しない。
- completionはworkspace、modal open generation、request generationでfenceする。close/reopen後のlate completionがscroll位置を戻したりmodalを再表示しない。再open時はoffsetを先頭へresetし、fresh observationを要求する。

## 受入条件

- runtime数が表示capacityを超えても、keyboardとwheelで全行へ到達でき、先頭/中間/末尾のindicatorとfooter hintが正しい。
- modalの背後にlive terminalがあっても、modal中のwheel/Up/DownはPTY inputやterminal scrollを一切変更しない。`Escape`は従来どおり即座に閉じる。
- modalを開いたままruntimeがreserved→live→exited/reclaimedへ変化すると、bounded cadence内に一覧へ反映される。
- slow/hung/unavailable daemonでもmodalのscroll・close・TUI quitは1 frame + scheduler誤差以内に進み、last-known snapshotにはstale/errorが明示される。
- inventory insert/remove/reorder、session rename/remove、duplicate-looking short IDs、late/duplicate completionでwrong rowへjumpせず、exact identity fenceを維持する。
- observation request/thread/queueとper-frame completion drainはhard boundを持ち、modalを長時間開いてもRPC/thread/memoryが増え続けない。

## 必須テスト

- 0/1/2/16+ runtime、0/1-row capacity、Up/Down/Page/Home/End/wheelのviewportとindicatorをtable-drivenに固定する。
- terminal scroll controlsを持つbackground pane上でdaemon modalを開き、scroll inputがmodalだけを変えることをassertする。
- fake clock + barrier portでperiodic refresh、one in-flight/coalesce、hang/failure/recovery、close/reopen、late completionを固定する。
- inventory先頭へのinsert、anchor runtimeのremove、session label変更、同じ短縮prefixを持つruntime IDでstable anchorを検証する。
- 実daemon fixtureでmodal表示中にAgent lifecycleを遷移させ、表示更新とEsc/quitのwall-clock boundを確認する。

## 根拠箇所

- `crates/tui/src/presentation/views/daemon_modal.rs`: `runtime_lines`, footer
- `crates/tui/src/usecase/application/controller.rs`: `Overlay::Daemon` input routing
- `crates/tui/src/presentation/mod.rs`: `refresh_agent_inventory`, `RestoreRetryState`, frame loop
- `crates/tui/src/presentation/views/workspace.rs`: `with_agent_inventory`, daemon modal projection
- `src/tui_input.rs` / `crates/tui/src/usecase/terminal_input.rs`: wheel classification
- `document/03-tui.md`: daemon status modal contract
