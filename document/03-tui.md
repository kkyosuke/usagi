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

Welcome は Open / Recent / New / Config の入口である。Open は登録済み workspace を名前の
大文字・小文字を区別しない alphabet 順に並べる。常時表示する Filter 欄は編集位置に cursor を
示し、入力した文字で即座に名前を絞り込み、↑↓ で絞り込み結果を選ぶ。各 workspace は名前と、session 数・未完了 issue 数・
最終更新の相対時刻を 2 行で表示する。Recent は同じ Workspace 画面を直接開く。New と Config は
それぞれの backend port を通じて作成・保存し、失敗時は入力中の draft を保持する。

実端末は raw mode、alternate screen、cursor、mouse、自動折返しを合成ルートで管理する。TUI は端末非依存の
event stream を reducer に渡し、frame diff だけを返す。TUI の実行中は自動折返しを無効化し、右下セルへの描画が
スクロールを起こさないようにする。resize は前 frame を無効化して全体を再描画し、終了時は端末属性、折返し設定、
alternate screen を復元する。

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
Overview の実行 port を通じて daemon IPC request になる。remove の target は表示名や入力値で再解決せず、
現在選択中の session record に限る。root を remove しようとした場合と daemon が request を受理できない場合は、
modal に安全な error を表示する。

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

右ペインは session 名の右に tab を Chrome 風の chip として描き、その直下に active marker を置く。path は
右ペインには表示しない。chip の表示順・label は表示専用であり、選択は pending の `OperationId` または live の完全な `TerminalRef` から投影する。
幅が狭い場合も ANSI を閉じた上で chip を clipping する。pending chip は v1 の選択 session と同じ
Nerd Font うさぎ `󰤇`（U+F0907）だけを
frame ごとに chip 内で進め、ラベル全体を着色しない。
tab が無い target は、灰色の静的うさぎと `No tabs stirring yet. Enter starts one.` の案内を、それぞれ
右ペイン幅の中央に表示する。描画前に clip して各灰色 SGR を reset で閉じるため、狭幅でも後続の
画面へ色が漏れない。この空状態は tick や runtime 接続に依存しない。overlay はこの Home frame を背景のまま合成する。

Closeup action で `agent` または `terminal` を確定すると、その pending tab を即座に選択して右ペインへ
表示する。`←` / `→`（または `h` / `l`）は tab を巡回し、`x` は選択 tab を閉じる。最後の tab を閉じると
Closeup action と空状態へ戻る。close は client-side selection を外すだけであり、daemon-owned terminal を
停止しない。

Closeup の `agent [profile]` は既存 session だけで実行できる。profile を省略すると daemon の
workspace policy を使い、指定時も product-neutral な profile ID だけを durable operation に渡す。
TUI は daemon の accepted response 後に Agent pending tab を置き、同じ operation の成功 final が返す
完全な `TerminalRef` にだけ attach する。daemon 不通、拒否、未知・古い completion では local spawn や
名前からの terminal 推測をしない。

daemon inventory、attach/resume、stream、resync は `pane_runtime` が結合する。output cursor に gap が
ある場合は local output を継ぎ足さず、daemon の atomic snapshot で置き換える。resize は geometry が
変化したときだけ送る。detach はこの client の subscription を外すだけで、PTY を kill しない。

`agent [profile]` は active な session だけを対象にする。profile を省略した request は daemon の
default policy に委ね、TUI は product 固有の argv、model、secret を組み立てない。controller が発行した
`OperationId` は pending tab と IPC request で同一のまま保持され、adapter は同じ ID の effect を一度しか
送らない。accepted の間は Agent pending tab を残し、replay を含む final は workspace と session が一致する
完全な `TerminalRef` のときだけ既存の `PaneRuntime` へ渡す。

```text
Closeup agent ─► LaunchAgent(operation, profile?) ─► daemon Agent request
       │                         │                         │
       │                         └─► pending Agent tab      └─► accepted / replayed final
       │                                                           │
       └─ root / invalid profile: safe inline feedback             └─► fenced TerminalRef ─► attach
```

transport failure、unknown / duplicate final、別 workspace または別 session の terminal final は local spawn、
request retry、attach を行わない。failure は pending tab を除去し、daemon が安全と保証した文言だけを
Closeup pane の feedback として表示する。

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
