---
number: 505
title: fix(v2): Claude/Codex Agent tab を usagi 終了・daemon 再起動後に復帰可能にする
status: todo
priority: high
labels: [review, v2, epic, tui, daemon, agent, recovery]
dependson: [504]
related: [209, 350, 388, 492, 503]
created_at: 2026-07-21T21:20:29.599700+00:00
updated_at: 2026-07-21T21:30:20.512145+00:00
---

## レビュー結果

v2 の Claude / Codex Agent tab について、現在の production path は要求を部分的にしか満たさない。

| シナリオ | 現在の挙動 | 判定 |
|---|---|---|
| TUI（usagi）だけを閉じ、同じ daemon のまま開き直す | daemon inventory から `live` runtime を tab へ再投影でき、Agent process は継続する | 部分達成。`PaneRegistry` 自体は memory-only で、tab 順序・選択・閉じた intent は失われる。restore は一度だけの同期処理で、選択外 tab も attach する |
| `usagi daemon restart` を実行する | [restart usecase](../../crates/daemon/src/usecase/restart.rs) は旧 daemon を stop してから新 daemon を start する。unfinished runtime は起動時 reconcile で `identity_unknown` になり、live inventory から外れる | 未達。#209 の active/draining rollover 契約と production command が接続されていない |
| daemon crash / cold restart 後に provider 会話へ戻る | provider-native resume 基盤はあるが、公開操作は managed session 単位で root を扱えず、複数の履歴を exact target で選べない | 未達。Claude は exact-target UI/API が不足し、Codex は加えて [#504](./504-feat-daemon-codex-structured-capture-wiring.md) の production capture が未実装 |

完了済み #388 は同一 daemon の live inventory 投影、#503 は provider-native identity の durable model、#492 は daemon 内の generation authority を扱う。本 finding は、それらを shipping lifecycle と product-level tab UX に接続する回帰修正であり、既存 issue を再起票しない。

なお、同番号の別 issue が存在するため、live pane restore の既存 epic は番号だけでなく [workspace restore epic のファイル](./390-feat-workspace-open-daemon-scope-live-agent-terminal-stable-identity-pane-tab.md) を正本として参照する。

## 復帰契約

「daemon 再起動後の復帰」は failure mode ごとに次のように定義する。

| failure mode | 復帰するもの | provider の再起動 |
|---|---|---|
| TUI close / reopen、daemon は同一 | 同じ `TerminalRef`、PTY、Agent process、screen/output cursor。保存済み tab intent と live inventory を reconcile する | しない |
| planned `usagi daemon restart` | 旧 draining generation が所有する同じ `TerminalRef`、PTY、Agent process。control authority だけを新 active generation へ移す | しない |
| crash、SIGKILL、cold stop/start、OS reboot | 旧 PTY の継続を偽らず interrupted tab を表示し、利用者が選んだ exact provider conversation を新しい runtime で明示 resume する | 明示操作時だけ行う |

TUI open、inventory restore、daemon restart を理由に Agent の作業を自動継続しない。provider resume は古い操作や外部副作用を再開し得るため、常に利用者の明示操作を要求する。

## 分割

- #506: live Agent tab の表示 intent を durable にし、TUI close / reopen 時に daemon inventory と reconcile する。
- #507: shipping `daemon restart` を active/draining generation rollover へ接続する。
- #508: draining generation の inventory と `TerminalRef` owner endpoint routing を client / IPC に接続する。
- #509: interrupted runtime を root / managed session の別なく exact target で列挙・resume できる daemon contract を追加する。
- #510: interrupted Claude / Codex を tab 単位で表示し、選択した会話だけを明示 resume する。
- 既存 #504: Codex の正式な structured session-ID capture を production に配線する。Codex の cold-restart 成功条件はこの issue の完了を必要とする。

## 受入条件

- [ ] Claude と Codex を同時に起動した workspace で TUI を正常終了して開き直すと、元の Agent tab が exact identity・順序・選択で一度だけ復元され、process / spawn count は変わらない。
- [ ] shipping `usagi daemon restart` 後も、旧 generation の Agent tab に双方向 IO でき、provider resume argv や replacement spawn が発生しない。restart 後の新規 Agent は新 active generation が所有する。
- [ ] old generation は最後の owned terminal が終了した後だけ回収され、stale / wrong-generation request は effect zero になる。
- [ ] crash / cold restart では旧 PTY を live と表示せず、root と managed session、同一 scope の複数履歴を別々の interrupted tab として表示する。
- [ ] 利用者が選択した exact interrupted tab だけを新 runtime へ resume し、TUI open / reconnect / restart から自動 resume しない。
- [ ] Claude は daemon 発行 ID、Codex は #504 の正式 structured capture で得た ID のみを使い、raw provider ID・argv・cwd を client 入力にしない。
- [ ] daemon 不通、corrupt/stale local state、scope / generation / adapter mismatch、provider metadata 不足を fail-closed に扱い、誤 attach・空会話・二重 spawn・二重 tab を発生させない。

## 必須 product E2E

in-process coordinator の再構築だけで完了とせず、shipping binary、実 daemon process / Unix socket / host PTY、長時間動作する Claude / Codex fixture executable を使う。

1. TUI から Claude / Codex を起動し、TUI close / reopen を 2 回行って exact tab・retained output・双方向 input・spawn count 1 を確認する。
2. 同じ 2 tab を保持したまま実際の `usagi daemon restart` を呼び、old/new generation、owner routing、drain 完了を確認する。
3. daemon を SIGKILL または明示 force-cold した fixture では live attach を拒否し、interrupted tab から選んだ provider conversation だけを新しい `TerminalRef` へ resume する。通常 `daemon stop` を cold failure の代用にしない。
4. root、複数 managed session、同一 session 内の複数履歴、Claude / Codex 混在、duplicate snapshot、stale ref、transport failure を含める。

既存の `tests/agent_ipc_e2e.rs`、`tests/cli_tui_pty.rs`、`tests/cli_tui.rs` の harness を拡張する。現在の empty data-dir restart test や reducer fake test だけを product acceptance の代用にしない。

## docs / 非目標

実装時に [TUI](../../document/03-tui.md)、[IPC](../../document/04-ipc.md)、[daemon](../../document/05-daemon.md)、[workspace pane restore proposal](../../document/proposals/11-workspace-restore-panes.md) を、上記 failure-mode matrix に合わせて更新する。

daemon crash 後も同じ PTY master fd を維持する broker / FD handoff は #221 の将来設計であり、本 epic の非目標とする。crash / reboot は provider-native explicit resume による新 runtime 復帰を保証する。
