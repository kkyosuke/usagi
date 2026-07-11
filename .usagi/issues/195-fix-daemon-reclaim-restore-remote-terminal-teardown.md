---
number: 195
title: fix(daemon): reclaim と restore無効終了で remote terminal を明示teardownする
status: done
priority: high
labels: [fix, daemon, tui, orchestration, review]
dependson: []
related: [167, 173]
parent: 159
created_at: 2026-07-11T01:30:35.010294+00:00
updated_at: 2026-07-11T06:20:29.148892+00:00
---

## 症状

Unixの通常経路ではpane backendをdaemonが所有する。次の明示的な所有権終了経路がremote terminalへ `Kill` を送らず、`DaemonTerminal::Drop` の `Detach` だけで終わる。

1. merged sessionのauto reclaim: UIは「Reclaimed」と表示しopen-panes snapshotを消すが、agent/shellはdaemon内で継続する。
2. `restore_panes_enabled=false` でTUI終了: pane snapshotを保存しない一方、terminalはdaemonに残るため、次回TUIからIDを発見できない。

`#173` はagent processを終了してRSSを回収する目的でdoneになっているが、daemon backendでは受け入れ条件を満たさない。

## 根本原因

- `TerminalPool::close_all` がsession mapをremoveするだけで、各 `PaneBackend::kill` を呼ばない。
- remote backendのDropは「TUI終了後もagentを継続する」ため意図的にdetach-onlyである。
- callerが「Drop=終了」と「Drop=detach」を区別していない。
- restore設定がpool teardown policyへ渡らない。

## 方針

- explicit close/reclaim/removeを共通のteardown APIへ集約し、remoteへKillを送る。
- daemonのKilled ackまたはprocess/registry消滅を確認してから成功表示・snapshot clearを行う。
- restore無効時のquit policyを「このTUIが開いたterminalをkill」にするか、別の永続owner registryで再発見可能にする。
- multi-client時に他clientがattach中のterminalを誤ってkillしない所有権規則を定義する。

## 受け入れ条件

- auto reclaim後にagent processとdaemon terminal registry entryが消える。
- reclaim失敗を成功表示しない。
- restore無効でopen→quit→openを繰り返しても到達不能terminalが増えない。
- restore有効時の通常quitは従来どおりagent継続・再attachできる。
- manual tab close/session removeの挙動は不変。

## テスト

- kill spyを持つremote backend unit test。
- heartbeat commandをspawnするdaemon E2Eでreclaim後の停止確認。
- restore on/off、multi-client、Kill ack failureのmatrix。
