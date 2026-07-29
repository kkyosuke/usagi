---
number: 579
title: test(tui): Workspace Agent drawer の shipping E2E と仕様更新を完了する
status: done
priority: high
labels: [v2, tui, test, e2e, docs]
dependson: [578]
related: [388, 506, 510, 545]
parent: 571
created_at: 2026-07-27T23:05:33.244753+00:00
updated_at: 2026-07-29T02:03:51.193304+00:00
---

## 背景

Epic #571 の機能実装は #575〜#578 に分割する。本 issue は各unit/integration seamだけでは保証できない **shipping TUI binary・実daemon/socket/PTYを通るend-to-end受入** と、実装完了後の仕様SSoT更新を所有する。

本issueで新しいproduct behaviorを設計し直さず、子issueで実装された契約をproduction compositionから検証し、残った旧root Closeup経路・テストfixture・docsを整理してEpicを閉じる。

## 対象責務

- shipping TUI、実daemon process / Unix socket / host PTY、fixture Agent CLIを使うprocess-level E2Eを追加する。
- root Agent drawerとmanaged-session Closeupを同時に持ち、drawer open/close前後のforeground handoff、exact identity、retained output、双方向input、resize、child PID/spawn countを検証する。
- rootに複数Agentを作成し、非先頭selection/reorder/close/dismiss/reopenを保存してnormal quit、abrupt TUI exit、fresh reopenを行い、exact tab/order/selectionとspawn count不変を検証する。
- daemon cold restart後にroot historyがinterruptedとして投影され、TUI open/inventory/drawer open/reconnectからprovider resume invocationが0、選択tabへの明示`Ctrl-O r`後だけreplacement spawn 1になることを検証する。
- CLI pickerでinstall済み各fixtureを選択し、explicit profile/root scope、double submit/replayの1 spawn/1 tab収束、0 CLIのsafe empty stateを検証する。
- session 0件、managed foregroundあり、narrow/resize、drawer open中のworkspace leaveをprocess E2Eで固定する。daemon outage、duplicate/conflicting inventory、wrong-scope final、persist/future-schema failureの全failure matrixは#577/#578のdeterministic testを正本とし、本issueでは代表ケースだけをproduction compositionから確認する。
- production compositionから旧root sidebar/Closeup/Terminal/Diff actionへ到達する経路が残っていないことを検証し、未リリース機能なので互換shimやmigration fixtureを残さない。
- `document/03-tui.md` の Home/target、sidebar、drawer、input、Agent CLI選択、pane restore/resumeを実装済み現在形へ更新する。必要な場合だけarchitecture/IPC/daemon docsへ「wire root scopeは不変」を記載し、事実を重複させない。
- issue #571 の受入条件を子issue/テストへ対応付け、Epic完了時に矛盾・未所有項目がないことを確認する。

## 必須シナリオ

1. **managed pane ↔ root drawer handoff**
   - managed Agent/Terminalをforegroundにする。
   - header buttonと実キー`Ctrl-O g`の双方でdrawerを開く。
   - root Agentをpickerから起動し、output/input/resizeを確認する。
   - drawer closeで同じmanaged exact tab、再openで同じroot exact tabへ戻り、両child PID/spawn count不変。
2. **durable conversation intent**
   - rootに複数provider conversationを作成。
   - reorder、非先頭select、dismiss/reopenを行う。
   - normal quitとTUI SIGKILL相当の双方からfresh reopenし、exact membership/order/selection、retained output、input echoを確認。
3. **cold restart / explicit resume**
   - daemonをcold failure相当で再起動。
   - old PTYをliveと表示せず、各historyをdistinct interrupted tabにする。
   - explicit action前のresume/spawn 0、選択1tabのresume後だけnew exact TerminalRef/spawn 1、他history不変。
4. **failure / boundary**
   - session 0、CLI 0、uninstalled default、daemon outage、duplicate/stale/wrong-scope final、narrow terminal、resize、drawer open中leave。
   - local spawn、二重tab、background input leakage、focus steal、panic、raw provider/daemon detail露出がない。

## 非対象

- provider transcriptの独自chat UI。
- daemon/coreのroot scope廃止。
- CLI installationや認証自動化。
- planned daemon rollover自体の実装（既存owner routing/recovery issueの責務）。

## 受入条件

- [ ] 上記shipping E2Eが実binary/process/socket/PTYでgreenになり、reducer fakeだけに依存しない。
- [ ] #575〜#578の受入条件がそれぞれunit/integration/E2Eへtraceでき、未テストのproduct pathがない。
- [ ] sidebar、header hit-test、drawer/input、restore/resume、picker/launchのproduction compositionが各entry pathで同一である。
- [ ] root Terminal / Diff /旧root CloseupへのTUI入口・fixture・docsが残らず、root paneはAgent-onlyである。
- [ ] `document/03-tui.md`と必要な関連docsが実装済み現在形になり、未実装予定や重複SSoTを含まない。
- [ ] fmt / clippy / selected tests、PR CIのfull test/coverage 100%/link checkがgreenである。
