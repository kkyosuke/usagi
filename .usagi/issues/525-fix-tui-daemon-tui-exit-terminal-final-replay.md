---
number: 525
title: fix(tui/daemon): TUI不在中にexitしたterminalのfinal replayへ到達可能にする
status: done
priority: high
labels: [review, v2, tui, daemon, terminal, replay, recovery]
dependson: []
related: [385, 386, 388, 473, 506, 510]
created_at: 2026-07-22T11:42:27.682373+00:00
updated_at: 2026-07-24T13:51:29.004604+00:00
---

## 問題・影響

daemonはprocess exit後にPTY transport/FDを解放しつつbounded final replay/tombstoneを保持できる。#386のinventoryはexit後runtimeも `live: false` で列挙するが、そのitemにfinal replay・exit status・観測状態がなく、#388のTUI restoreはnon-live itemをskipする。さらにTUIが接続中なら#385のexit検知直後にtabを自動削除する。TUIを閉じている間、launch workerがportを手放している間、またはtemporary disconnect中にAgent/generic Terminalがexitすると、reopenしたUIからtombstoneと最終出力へ到達する経路がない。

## 既存issueとの境界

#386はunified inventoryでnon-live itemを列挙し、#388はlive restoreだけを行う。本issueは既存inventoryのliveness契約を変えず、exited finalのquery/projectionを追加する。#385は接続中exit検知、#473はPTY map/FD回収とfinal replay分離を所有する。本issueはretained tombstoneをproduct UIへ投影するconsumer/IPC経路。

#506 active writerはlive Agent tab intentとcontinuation-scoped dismissal、#510はcold-restart後のinterrupted provider resumeを所有する。本issueはそれらのintentを上書きせず、provider resume/spawnを行わない。aggregate retention/GCは#526。

## durable visibility scope / merge契約

- final visibilityはclient-localではなく、**同じlocal userのworkspace-global durable state**とする。複数TUI process/reopenが同じ結果へ収束する。
- primary keyはdaemon generation、terminal ID、workspace ID、optional session ID、worktree IDを全て含むexact `TerminalRef`。名前、pane、continuationだけで別incarnationへfallbackしない。
- 各keyのvisibility stateは `Unobserved < Observed < Dismissed` のmonotonic latticeとrevisionを持つ。`Observed` / `Dismissed` commandはexpected revisionを伴うCAS、同値retryはidempotent、conflictはauthoritative snapshotを返してmax-stateへmergeする。
- late `Observed`、stale inventory、out-of-order durable writeは `Dismissed` を下げず、completed tab/notificationを復活させない。別exact `TerminalRef`のstateは独立する。
- inventory/queryはvisibility revisionを含み、TUIはprojection適用前後のCAS conflictをrefreshしてmonotonic mergeする。process-local「既に見た」flagをauthorityにしない。

## #506 dismissal precedence

- live Agent tabが#506のcontinuation-scoped dismissal済みなら、そのruntimeが後でexitしてもfinal tombstoneをcompleted tab/notificationとして自動再表示しない。#506 suppressionがauto-projectionに優先する。
- suppressed finalは破棄せずretention期間保持し、historyからexact `TerminalRef`を選ぶ明示reopen時だけread-only final replayを表示できる。明示reopenは#506のlive continuation intentやreplacement spawnを復活させない。
- #506 dismissal lineageとexact terminal visibilityのどちらかがauto-showを禁止する場合は非表示へmonotonic mergeする。別incarnationへsuppressionを拡張するかは#506 contractだけが決め、本issueは推測しない。

## 対象責務

- scope-filtered inventory/queryでexited tombstoneのexact `TerminalRef`、final output offset、exit status、bounded replay locator、kind、retention identity、visibility state/revisionを返す。
- TUI reopen/reconnect時にworkspace-globalでunobservedかつ#506 suppressionされていないfinalをcompleted/read-only tabまたは履歴UIへstable identityで一度だけ投影する。
- live→exited transition中のlate outputをexitより先に保持し、接続中の即時auto-closeでfinalを到達不能にしないUXへ変更する。
- observed/dismissed CASをdurableにし、duplicate inventory/reopen/multi-TUIで再通知・二重tab・resurrectionを作らない。dismissはterminal/processを変更しない。
- stale scope/wrong generation/partial inventoryでは別terminalへfallbackせず、provider resumeやreplacement spawnを発火しない。
- aggregate GCは#526へ委譲し、本issueはminimum visibility/observed/dismissed/pin contractを公開する。

## 受入条件

- [ ] TUI不在中にAgent/generic Terminalがfinal output後exitしても、fresh TUIからexact tombstoneと最終画面/exit結果を閲覧できる。
- [ ] connected exit、disconnect race、reopen、duplicate inventoryでfinal outputは欠落/重複せず1つのstable completed entryへ収束する。
- [ ] 2つのTUIが同じrevisionからobserve/dismissし、writeがout-of-orderでもworkspace-global stateはmonotonic `Dismissed`へ収束し、late writerがcompleted tabを復活させない。
- [ ] close/dismiss後は同じexact finalが勝手に再表示されず、別TerminalRef incarnationを誤抑止しない。
- [ ] #506 continuation-scoped dismissal済みAgentがexitしてもauto-showせず、明示history reopen時だけread-only finalを表示する。
- [ ] completed entryはread-onlyでinput/resize/live Resume/spawnを送らない。
- [ ] #506/#510のlive/interrupted Agent tabとidentity衝突せず、generic Terminalも同じ到達性を持つ。

## 必須product test

実daemon/PTYでTUI attach→TUI終了→terminalがunique final markerを出してexit→fresh TUI open→marker/status表示→dismiss→second reopenで非再表示を検証する。さらに2 TUIのsame-revision observe/dismiss CAS、dismiss先着/後着、stale inventory、out-of-order write/retry、別TerminalRef incarnationをdeterministic barrierで検証する。#506 dismissed live Agent→exitではauto-show 0、explicit history reopen 1、spawn/resume 0をassertする。Agent/generic、root/session、exit/disconnect race、late final chunk、wrong scopeを含める。

## docs / migration

`document/03-tui.md` にcompleted/read-only UX、workspace-global visibility、#506 dismissal precedenceを、`document/04-ipc.md` にexited inventory/queryとrevision/CAS/monotonic mergeを、`document/05-daemon.md` にexact-key tombstone lifetime boundaryを記載する。
