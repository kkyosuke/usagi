---
number: 672
title: fix(tui): Agent tab intent 永続化を input/render thread から分離する
status: todo
priority: high
labels: [review, v2, tui, daemon, agent, uiux, input, persistence, responsiveness]
dependson: []
related: [506, 551, 577, 665]
parent: 664
created_at: 2026-08-06T22:23:04.741837+00:00
updated_at: 2026-08-06T22:23:04.741837+00:00
---

## Finding（P1 input responsiveness / local durable IO）

daemon-owned Agent paneの表示intent（tab順・選択・dismiss/reopen）はuser-local `agent-tabs.json`へatomic commitされるが、そのload/mutationはTUIのinput/render threadから同期実行される。

- workspace entryの `WorkspaceUi::with_agent_tab_intent` は最初のHome frame前に `AgentTabIntentPort::load`を呼ぶ。
- Director/managed paneのtab切替、reorder、reopen、pane completion、restore observationは `WorkspaceUi::mutate_agent_intent`からportを同期呼び出しする。選択/reorderは入力を1件読んだ直後、completion/observationは次frameのdraw前である。
- production `UserAgentTabIntentPort` は `FileAgentTabIntentStore`へ直結する。
- storeはprivate directory検証/作成、cross-process exclusive file lock、既存JSON全読込・parse、CAS/reducer、temporary file全write、`file.sync_all()`、rename、parent directory `sync_all()`を同じcall内で行う。

通常は小さいfileでも、別TUIがlockを保持する、data directoryが遅い/network filesystem上にある、fsyncが停滞する、corrupt quarantine/permission検証が遅い場合にwall-clock boundがない。tab切替を押した瞬間、またはAgent launch/restore completionが届いたframeで、draw・次input・scroll・daemon modal・quitが止まる。#665がterminal RPCをworkerへ移しても、このlocal durable IOは別経路で残る。

## 修正方針

- workspaceごとにsingle-writerなAgent tab intent persistence actorを置き、load/read-modify-write/file lock/fsyncをTUI threadから外す。frame loopはbounded request enqueueとcompletion drainだけを行う。
- 初期load中も最初のHome frame、resize、leave/quitを処理する。load完了前はintent-dependent restore/admissionを推測せず、明示loading stateへ保持する。missing/corrupt/future-schema/permission errorの既存fail-closed契約を維持する。
- mutation requestはworkspace、expected durable revision、typed mutation、client operation id、UI/open generationを持つ。completionだけがcommitted `AgentTabIntent`とCAS結果を更新し、late/duplicate/別workspace completionをdropする。
- 現行の「durable commit失敗時はclose/reorder/selection/reopenの可視stateを成功扱いで変えない」を維持する。入力受付直後はpending marker/feedbackを出しても、selection/order/dismissalの確定表示は成功completion後だけにする。
- queueは件数/bytesともhard boundを持つ。未開始の同一target `Select` / `Reorder`は安全にlatestへcoalesceできるが、`Dismiss` / `Reopen` / `Upsert` / `ObserveAll`の因果順序を落としたり入れ替えたりしない。backpressureはtyped noticeで、silent successにしない。
- cross-process file lock、revision/CAS merge、causal Dismiss-vs-Reopen、atomic temp+fsync+rename、corrupt quarantine、future-schema read-onlyをSSoTのまま保つ。actor内cacheだけを権威にせず、各commitは最新durable stateをlock下で読む。
- completion drainは1 frameあたりの件数/time budgetを持つ。shutdown/leaveは新規admissionを閉じ、未開始requestを失敗へ収束し、in-flight filesystem syscallの完了を無期限joinしない。未確認commitを成功表示しない。

## 受入条件

- Agent tab intentのload、exclusive lock待ち、read、file fsync、parent fsyncの各段階が停止しても、Home初期frameまたは次frame、resize、scroll、daemon modal、leave/quitが1 frame + scheduler誤差以内に進む。
- tab select/reorder/reopen/dismiss/upsertはdurable成功後に一度だけ可視stateへ反映され、失敗・CAS conflict・late completionでsuccessを捏造しない。
- concurrent TUIのSelect/Reorder/Dismiss/Reopen、stale Observe、replacement TerminalRefが現行merge契約へ収束し、lost update・復活・wrong focusを起こさない。
- input連打、launch/restore completion burst、queue fullでもrequest/thread/memory/per-frame drainがhard bound内に留まり、causal mutationをsilent dropしない。
- missing/corrupt/future schema、unsafe symlink/hardlink/mode、revision exhaustion、publish前failureの既存bytes保全・typed feedbackを維持する。
- workspace切替/close後のcompletionが新workspaceのtab intent、notice、focusを変更しない。

## 必須テスト

- barrier付きfake portでloadと各mutationをhangさせ、draw/input/resize/quitの進行とpending feedbackをassertする。
- real `FileAgentTabIntentStore`を2 writerで競合させ、一方がexclusive lock中でもTUI driverが進むこと、解放後にCAS/causal mergeへ収束することを固定する。
- injectable file publisherでread/write/file sync/rename/parent sync/quarantine failureを1段ずつ発生させ、可視stateとold bytesが維持されることを検証する。
- Select/Reorder coalesce、Dismiss/Reopen順序、queue full、workspace/open generation変更、late/duplicate completionをtable-drivenに固定する。
- shipping TUI + daemon fixtureでAgent tab切替/並べ替え中に別processがintent lockを保持し、Esc/quit wall-clock boundと解放後の永続順序を確認する。

## 根拠箇所

- `crates/tui/src/presentation/mod.rs`: `with_agent_tab_intent`, `mutate_agent_intent`, tab select/reorder、pane/restore completion
- `src/runtime/tui.rs`: `UserAgentTabIntentPort`
- `src/runtime/agent_tab_intent.rs`: `with_lock`, `load`, `mutate`, `write_unlocked`
- `crates/tui/src/usecase/application/agent_tab_intent.rs`: durable mutation / CAS contract
- `document/03-tui.md`: Agent tab intent restore / persistence contract
