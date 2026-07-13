# 3. TUI

> [ドキュメント目次](README.md) ｜ ← 前へ [2. アーキテクチャ](02-architecture.md) ｜ 次へ → [4. daemon IPC](04-ipc.md)

v2 TUI の現在の画面遷移、live pane、および TUI-local resume state の仕様である。daemon
の resource schema や wire protocol は本書では所有せず、[4. daemon IPC](04-ipc.md) と
[5. daemon](05-daemon.md) を境界の正本とする。

## 目次

- [画面と入力](#画面と入力)
- [Home と target](#home-と-target)
- [Overview と modal](#overview-と-modal)
- [Sidebar mascot](#sidebar-mascot)
- [Closeup pane](#closeup-pane)
- [resume data compatibility](#resume-data-compatibility)
- [feedback と終了](#feedback-と終了)

## 画面と入力

Welcome は Open / Recent / New / Config の入口である。Open は登録済み workspace を選択し、
Recent は同じ Workspace 画面を直接開く。New と Config はそれぞれの backend port を通じて
作成・保存し、失敗時は入力中の draft を保持する。

実端末は raw mode、alternate screen、cursor、mouse を合成ルートで管理する。TUI は端末非依存の
event stream を reducer に渡し、frame diff だけを返す。resize は前 frame を無効化して全体を再描画し、
終了時は端末属性と alternate screen を復元する。

## Home と target

Home の navigation target は `Root(WorkspaceId)` または `Session(SessionId)` である。表示名と
配列 index は identity に使わない。selected は cursor、active は command と Closeup の対象であり、
cursor の移動だけでは active を変更しない。

daemon snapshot で selected または active の session が見つからなくなった場合、両方を同じ
workspace の root へ戻す。これにより、削除済み session を target にした古い local state を実行に
使わない。

Home の mode は Switch と Closeup である。Overview、Closeup action、PR、preview、diff、text、notes、
environment は Home の背景を残す overlay として開き、最前面の overlay が入力を受け取る。

Home controller の management input では、Switch の `Ctrl-A` は新規 session 作成フォームを開く。Closeup
の `Ctrl-A` は active target の Closeup action overlay を開き、作成フォームを開かない。Closeup の `Ctrl-O`
は Switch へ戻り、Switch 中の `Ctrl-O` は mode を変えない。daemon-owned live pane の同じ control bytes は
`LiveInputClassifier` が pane navigation として予約するため、この management transition に渡さない。

## Overview と modal

Overview palette の Tab は選択中のトップレベル command を補完する。`session` の第 1 引数は
登録済み subcommand の一意な prefix を補完するため、`session c` は `session create` になる。未知または
曖昧な prefix は入力を変えない。

`session create <name>`、`session list`、`session overview`、`session remove [--force]` は
controller で typed lifecycle effect に変換する。create は既存の create form と同じ producer-issued
`OperationId` / pending token を発行し、list / overview は daemon snapshot refresh を依頼する。remove は
表示名を identity に使わず、現在選択中の stable `SessionId` だけを削除対象にする。root を remove する入力は
effect を発行せず安全な notice にする。

modal は view ごとに予約した body 行数で描画する。候補数、empty state、result、error、loading、editor の
内容が変化しても、開いている modal の枠高さは変わらない。端末が短い場合は予約領域を安全に clip し、
Home 背景との合成範囲を越えない。

## Sidebar mascot

Home の左 sidebar は footer の直上に 3 行の usagi を表示する。frame は reducer が所有する tick でだけ
進み、瞬きと耳の動きは純粋 render で決まる。狭いペインでは menu の viewport を優先して mascot を省略し、
表示する場合も幅に clip する。modal は Home frame の上に合成されるため、mascot は背景の一部として残る。

## Closeup pane

Closeup tab は pending operation または live `TerminalRef` を持つ。pending completion は同じ
`OperationId` にだけ対応し、live tab は完全な `TerminalRef` で識別する。選択中の live tab だけを
attach し、選択外の tab は background のまま保持する。

右ペインは tab を Chrome 風の chip と、その直下の active marker で描く。chip の表示順・label は
表示専用であり、選択は pending の `OperationId` または live の完全な `TerminalRef` から投影する。
幅が狭い場合も ANSI を閉じた上で chip を clipping する。tab が無い target は、静的うさぎと
`No tabs stirring yet. Enter starts one.` の案内を中央に表示する。この空状態は tick や runtime 接続に
依存しない。overlay はこの Home frame を背景のまま合成する。

daemon inventory、attach/resume、stream、resync は `pane_runtime` が結合する。output cursor に gap が
ある場合は local output を継ぎ足さず、daemon の atomic snapshot で置き換える。resize は geometry が
変化したときだけ送る。detach はこの client の subscription を外すだけで、PTY を kill しない。

## resume data compatibility

TUI-local resume state が持てる terminal identity は完全な `TerminalRef` だけである。表示名、path、
単独の terminal ID から terminal を探し直したり、新しい terminal を spawn したりしない。

| 復元時の入力 | 判定 | fallback |
|---|---|---|
| saved target が snapshot に無い | target identity が stale | selected / active を root に戻す |
| saved `TerminalRef` が inventory に無い、または exited | attach 不可 | tab を除去し Closeup へ縮退する |
| terminal ID が同じでも daemon generation など fencing field が異なる | old / stale data | tab を除去し attach しない |
| attach / resync が ownership unknown または transport failure | 継続性を証明できない | safe feedback を表示し input を無効化する |

この migration は旧値を推測変換しない fail-closed policy である。TUI-local data は表示・選択の
復元候補に限られ、terminal、PTY、session mutation の所有権は daemon に残る。

## feedback と終了

phase、operation / terminal error、disconnect、reconnect、resync は safe message と error ID だけを
TUI-local feedback として表示する。transport の内部 detail や secret は表示しない。orphan state では
terminal input を送らない。

live pane がある場合の quit は確認を通し、確定後は detach だけを実行する。TUI の終了は daemon-owned
terminal を終了させない。
