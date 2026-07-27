---
number: 577
title: feat(tui): Workspace Agent drawer に root Agent conversation を復元・接続する
status: todo
priority: high
labels: [v2, tui, agent, recovery, persistence]
dependson: [576]
related: [388, 506, 510, 545]
parent: 571
created_at: 2026-07-27T23:04:06.141688+00:00
updated_at: 2026-07-27T23:04:06.141688+00:00
---

## 背景

#575 で managed-session navigation から workspace root を分離し、#576 で Workspace Agent drawer shell と入力所有境界を追加する。本 issue は既存の root scope Agent 基盤（`session_id: None`、live/interrupted inventory、`AgentTabIntent`、exact resume、daemon-owned PTY）を drawer に接続する。

本 issue の主眼は **前回 conversation の復元、Agent-only invariant、foreground attach/input/resize、明示 resume** である。新しい conversation を起動するCLI pickerは #578 が所有する。
既存の `AgentTabIntent` reconcile、pane reducer、exact resume contractを再実装せず、root drawerへのprojection・foreground
handoff・admissionの配線に限定する。

## 対象責務

- coherent inventory restore と root target の `AgentTabIntent` をreconcileし、drawerを閉じた状態でもroot conversationのorder/selection/dismissalを準備する。drawerは自動openしない。
- drawer open時、保存済みselected conversationがtrusted liveなら同じexact `TerminalRef`を選択し、foregroundになった1tabだけattach/resyncする。
- saved selectionがinterrupted/resumableなら同じ`AgentContinuationRef`のinterrupted tabを選ぶ。open/reconnect/restoreからprovider resumeを発火しない。
- saved selectionが消失した場合は同targetの次のsurviving slot、なければ先頭、conversationなしならempty stateへ決定的に縮退する。
- conversation切替、reorder、close/dismiss、reopen、selectionをroot `AgentTabIntent`の既存atomic/CAS契約でcommitする。失敗時は可視stateとbytesを成功扱いで変更しない。
- drawerはroot Agentのlive/pending/interrupted tabだけを受け入れる。root generic Terminal、Diff、Terminal pending/actionを作成・復元・表示できないよう、projectionだけでなくpane/runtime admissionにもAgent-only invariantを置く。
- selected live Agentへ既存VT projection、input ordering/ACK、scroll、selection/copy/link、detach/reconnectを接続する。
- drawer固有viewport geometryでroot Agent PTYとlocal VT screenをresizeする。背景managed Closeupのgeometryを送らない。
- drawer close時はroot foreground subscriptionをdetachし、開く前のmanaged-session selected foreground tabをattach/resyncする。どちらのPTY/processもkill/spawnしない。
- selected interrupted tabの`Ctrl-O r`を既存exact resume pathへ接続する。同じoperation/source/relation/lineage/root scope/new exact TerminalRefが一致した成功だけを同slotのlive tabへ置換する。
- daemon outage、partial/conflicting inventory、stale delayed restore、wrong scope/generation、persist/future-schema failureをfail closedに扱い、last valid intentを空で上書きせず、local spawn・自動resume・二重tab・focus stealを起こさない。

## 復元規則

| 入力 | 動作 |
|---|---|
| saved exact refがtrusted live root Agent | 保存slotへlive tabを1枚投影 |
| saved refはnon-live、同lineageがresumable | 同slotへinterrupted tabを投影。自動resumeなし |
| inventory-only live/interrupted Agent | continuation/exact refの決定的順序でappend |
| dismissed lineage | live/interruptedを表示しない。明示reopenだけが解除 |
| duplicate exact row | 1枚へdedup |
| conflicting row / duplicate live lineage /非全単射 | observation全体を拒否してretry |
| transport/partial outage | last valid projectionを保持しreconnecting表示 |
| future intent schema | read-only、mutation/resumeを拒否しbytes保持 |

## 非対象

- New Agent CLI pickerと新規launch（#578）。
- provider transcriptの独自chat renderer。
- daemon/coreのroot scope変更。
- shipping process E2Eと全体docs確定（#579）。

## 受入条件

- [ ] TUI close/reopen後、前回selected root live Agentをexact identity/order/selectionで復元し、PID/spawn countを増やさずretained output/inputを継続できる。
- [ ] drawer open時だけselected root Agentがforeground attachされ、他root tabとmanaged paneはbackground/detachedになる。close後は元managed foregroundへ戻る。
- [ ] interrupted root Agentは同slot/selectionで表示され、open/reconnect/inventoryではresume 0、明示`Ctrl-O r`だけがexact replacementを1回spawnする。
- [ ] root generic Terminal / Diffの作成・復元・表示effectが存在せず、root paneはAgent-onlyである。
- [ ] selection/reorder/close/reopen/dismissalがdurableで、duplicate/stale inventoryとconcurrent CASでもlost update・二重tab・focus stealを起こさない。
- [ ] drawer geometryでのみroot PTYをresizeし、背景managed paneのgeometry/inputを誤送信しない。
- [ ] outage、partial/conflict、wrong scope/final、persist failure、future schemaをfail closedに扱い、既存projection/intent/bytesを壊さない。

## 必須テスト

- pane/reconcile reducer: live/interrupted/absent/dismissed/duplicate/conflict/root-only filtering。
- runtime: foreground handoff、attach/detach/resync、input/resize geometry、stale revision/interaction/subscription fence。
- persistence: root target order/selection/dismiss/reopen、CAS競合、write failure、corrupt/future schema。
- integration: same daemonでnormal reopen / abrupt TUI exit、cold daemon restart後のexplicit exact resume。shipping PTYの最終受入は#579。
