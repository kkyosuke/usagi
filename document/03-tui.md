# 3. TUI

> [ドキュメント目次](README.md) ｜ ← 前へ [2. アーキテクチャ](02-architecture.md) ｜ 次へ → [4. daemon IPC](04-ipc.md)

v2 TUI の現在の画面遷移、live pane、および TUI-local resume state の仕様である。daemon
の resource schema や wire protocol は本書では所有せず、[4. daemon IPC](04-ipc.md) と
[5. daemon](05-daemon.md) を境界の正本とする。

## 目次

- [画面と入力](#画面と入力)
- [Home と target](#home-と-target)
- [Session sidebar rows](#session-sidebar-rows)
- [Overview と modal](#overview-と-modal)
- [Sidebar mascot](#sidebar-mascot)
- [Closeup pane](#closeup-pane)
- [Closeup Agent の手動確認](#closeup-agent-の手動確認)
- [resume data compatibility](#resume-data-compatibility)
- [feedback と終了](#feedback-と終了)

## 画面と入力

Welcome は Open / Recent / New / Config の入口である。Open は登録済み workspace を名前の
大文字・小文字を区別しない alphabet 順に並べる。常時表示する Filter 欄は編集位置に cursor を
示し、入力した文字で即座に名前を絞り込み、↑↓ で絞り込み結果を選ぶ。各 workspace は名前と、session 数・未完了 issue 数・
最終更新の相対時刻を 2 行で表示する。Recent は同じ Workspace 画面を直接開く。New と Config は
それぞれの backend port を通じて作成・保存し、失敗時は入力中の draft を保持する。

フォーカス中で編集可能な 1 行入力は共通の block cursor を使う。挿入位置の Unicode scalar を
入力値と同じ意味色の reverse-video で示し、空欄または行末では反転した空白 1 セルを示す。
この表示は文字を横へ押し出さず、全角文字も 1 scalar 単位で扱う。非フォーカス値、読み取り専用値、
候補・選択行の強調はそれぞれの既存表示を維持する。

対話的な `usagi` / `usagi hop` の Welcome 起動時は、入力を読まずに 110ms 間隔で 13 フレームの
スプラッシュを再生する。ピンクの usagi を先に表示し、`USAGI` を暗い緑から Success の太字へ
フェードインしてから Welcome を描く。スプラッシュ中の打鍵は Welcome の最初の入力として残る。
非対話環境と `usagi config` はスプラッシュを再生しない。

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

Home の mode は Switch と Closeup である。Overview、Closeup action、PR、preview、text、notes、
environment は Home の背景を残す overlay として開き、最前面の overlay が入力を受け取る。diff は
Closeup pane の tab として開く。

左 sidebar の marker は Home target 表示の正本である。Switch では selected cursor と current
target を別々に stable identity から照合し、同じ行なら cursor を優先する。Switch の cursor ではない
root / session 行は v1 と同じ dim の非アクティブ色で描き、selected の Accent と `+ new session` の
Success は保つ。Closeup では cursor を
描かず、current marker だけを残す。session cursor はうさぎ `󰤇` と太字の名前、main と `+ new session`
の cursor は `>`、cursor ではない current target は緑の `▎` で示す。`+ new session` と pending
skeleton は current target にならない。名前・補足・marker は ANSI を閉じた表示幅で clip/pad するため、
CJK、Nerd Font glyph 未対応、極小幅でも後続行の style や列幅を壊さない。

Home controller の management input では、Switch の `Ctrl-A` は新規 session 作成フォームを開く。Closeup
の `Ctrl-A` は active target の Closeup action overlay を開き、作成フォームを開かない。Closeup の `Ctrl-O`
は Switch へ戻り、Switch 中の `Ctrl-O` は mode を変えない。daemon-owned live pane の同じ control bytes は
`LiveInputClassifier` が pane navigation として予約するため、この management transition に渡さない。

Closeup の入力所有者は tab の有無で決まる。tab が無い Closeup は management input が所有し、action modal を
前面に出す。tab が 1 つ以上ある Closeup は `LiveInputClassifier` の `Ctrl-O` prefix（leader）が所有し、非
prefix の打鍵は live terminal への passthrough として扱う（`Ctrl-O`・`Ctrl-^` 以外は予約しない）。prefix の
follow-up は下表のアクションに解決する。

controller reducer path も同じ投影を使う。`LivePaneAvailability` が無い Closeup への遷移は action overlay を
自動で開き、pane が到着すると通常の tab surface へ戻る。adapter は prefix の next / previous 結果を
`CtrlN` / `CtrlP` として reducer に渡し、reducer は pane 所有者へ tab selection effect を要求するだけで、tab
identity は保持しない。

| prefix | アクション | 効果 |
|---|---|---|
| `Ctrl-O` `o`（または `Ctrl-O`） | Switch | Closeup から Switch へ戻る |
| `Ctrl-O` `a` | OpenCloseupModal | Switch では選択 target の Closeup action を開く。Closeup では tab があっても action modal を前面に出す |
| `Ctrl-O` `n` / `→` | NextTab | 次の tab を選ぶ |
| `Ctrl-O` `p` / `←` | PreviousTab | 前の tab を選ぶ |
| `Ctrl-O` `g` | Agent | agent pane を開く／再接続する |
| `Ctrl-O` `x` | CloseTab | 選択中の tab を閉じる |
| `Ctrl-O` `q` | QuitConfirmation | TUI を閉じる確認を開く |

leader は 1 秒で失効し、未知の follow-up は 1 打鍵だけ握って捨てる。`Ctrl-C` と `Ctrl-Q` は prefix より先に扱う。

## Session sidebar rows

Home sidebar は `main → divider → session* → + new session` の順序と target identity を保つ。main と作成 action は
1 行、各 session は固定 2 行で描画する。`Sessions` 見出しは表示せず、session が 0 件でも main の直後に divider を置く。作成中の skeleton は `+ new session` の直前に置く。session の 1 行目は cursor / active marker、表示名、常に幅を
予約する note icon を表示する。note icon は既存の text overlay を開く入力を増やさず、内容の有無だけを示す。

2 行目は daemon snapshot の `last_active`、または旧 record の `created_at` を基準に、`now`、`12m ago`、`3h ago`
のような相対時刻で表示し、dismissed でない PR があれば先頭の PR 番号と残り件数を続ける。Git の検査が完了した session は、remote の既定 branch（`origin/HEAD`）を優先した base との差分として `↑ahead ↓behind · +added -removed` を続ける。検査は sidebar の描画とは別スレッドで行い、完了後は 1 秒以上あけて現在の session 集合を再検査する。未完了・取得不能・意味を持たない base branch 自身の状態は表示しない。PR title の解決はこの行の前提にしない。snapshot に無い
session は selected / active を main に縮退させ、空一覧でも main と作成 action は残る。

Switch で `+ new session` を選び Enter（または `t`）を押すと、その行が `+ new: <name>` の
inline 入力欄へ置き換わる。名前を入力して Enter を押すと通常の `session create <name>` と同じ daemon
request を非同期に開始し、完了まで行の直前に session と同じ 2 行の skeleton を表示する。skeleton の activity glyph と session 名は同じ
左から右へ流れる低速の wave で描き、静的な点滅にはしない。daemon が同一 `OperationId` と revision を持つ `session.created`
完了 hook を返したときだけ、skeleton をその response 内の snapshot row に置き換えて loading を終了する。`c` と IME に依存しない `Ctrl-A` も
同じ inline 入力を開く。`Ctrl-A` は選択カーソルも `+ new session` 行へ移動する。Esc は入力を取り消し、空の名前は行の下に error を表示する。
完了 snapshot は sidebar row と daemon-issued session ID を同時に置換するため、`a` のような短い名前も
表示名ではなく stable ID で後続の Agent / terminal request を送る。snapshot の schema が不正な場合は raw
IPC body を画面やログへ出さず、安全な error を画面に表示して `<data dir>/logs/error-YYYY-MM-DD.log` に schema
error を記録する。

GIF はこの projection に含めない。diff の詳細表示や実行 shortcut は実行可能な daemon command が無いため追加せず、sidebar は read-only の Git summary だけを表示する。既存の Closeup / overlay の入力所有者と操作だけを維持する。

狭幅では cursor / active marker、表示名、note icon を優先し、補足行を ANSI-safe・Unicode display width 準拠で
clip する。viewport と作成中 skeleton は session ごとの 2 行 footprint を使い、mascot の予約より選択中 row を優先する。

## Overview と modal

Overview palette の Tab は選択中のトップレベル command を補完する。`session` の第 1 引数は
登録済み subcommand の一意な prefix を補完するため、`session c` は `session create` になる。未知または
曖昧な prefix は入力を変えない。

Config の `Modal mode` は Overview と Closeup の command surface に共通して適用される。`Action` は
入力欄を command filter として使い、`↑`/`↓` で候補を選択して Enter で実行する。`→` は選択した
command の subcommand picker を開き、`←` は閉じる。`Prompt` は入力した command line を Enter で解釈・実行する。

`session create <name>`、`session list`、`session overview`、`session remove <name> [--force]` は
Overview の実行 port を通じて daemon IPC request になる。この実行 port は起動経路に依存せず、
Welcome→Open・Welcome の Recent・direct な Workspace entry のいずれで開いた workspace でも同じ
daemon-authoritative な port を通る。screen graph は workspace 起動ごとに port を新しく生成し、
daemon の snapshot revision を workspace 間で持ち越さない。remove の target は command の name に限定し、
現在選択中の session record や root を暗黙に使わない。daemon が request を受理できない場合は、
modal に安全な error を表示する。
Closeup の `close [-f|--force]` は同じ checklist を開き、`-f` と `--force` は同値である。target、未知 flag、
重複 flag は安全に拒否する。

`session remove -s [--force]`（`--select` も同義）は、現在選択中の row を即時削除せず、中央の
session checklist を開く。`↑`/`↓` または `j`/`k` で cursor を移動し、Space で複数 row を選び、Enter で
選んだ session の削除を開始する。Esc は選択を捨てて元の Switch / Closeup surface に戻る。空一覧、未選択の
Enter、modal 表示中の背景入力は安全な no-op であり、追加の確認 step はない。modal は開いた snapshot の
`name`、`root`、`created_at` を entry の incarnation fence として保持する。refresh により一致しない entry は
request 前に除外するため、同名再作成や一覧更新で別の session を削除しない。

modal は view ごとに予約した body 行数で描画する。候補数、empty state、result、error、loading、editor の
内容が変化しても、開いている modal の枠高さは変わらない。端末が短い場合は予約領域を安全に clip し、
Home 背景との合成範囲を越えない。

## Sidebar mascot

Home の左 sidebar は footer の直上に usagi を表示する。frame は reducer が所有する tick でだけ
進み、瞬きと耳の動きは純粋 render で決まる。mascot block の直下には常に 1 行の空行を予約し、footer、
session viewport、pending row と重ならない。狭いペインでは menu の viewport を優先して mascot block 全体を
省略する。

presentation が表示安全な message を供給した場合だけ、mascot の上に黄色太字の角丸 speech bubble を出す。
bubble は `╰─┬─╯` の tail を mascot の頭へ向け、Unicode 表示幅で折り返し、各行を sidebar 幅に clip する。
message が無いときは無言の mascot のままで、renderer はダミー文言を生成しない。bubble と mascot は装飾であり、
入力 focus や terminal tab の input owner を取得しない。modal は Home frame の上に合成されるため、mascot は背景の
一部として残る。

## Closeup pane

Closeup pane の tab state は target-scoped registry が正本である。workspace root と各 session は同じ
registry API の別 entry を持ち、entry は pending、live tab、stable selection、forced action modal state を
所有する。session の切替は entry を破棄しないため、session A の create / completion / exit / close は session B
の tab、選択、modal state を変えない。background target の event はその entry だけを還元し、表示中 target の
attach や Closeup 遷移を発生させない。

Closeup tab は pending operation、live `TerminalRef`、または terminal を持たない完了済み document を持つ。pending completion は同じ
`OperationId` にだけ対応し、terminal live tab は完全な `TerminalRef`、完了済み document tab は operation で識別する。表示中 target の選択中 live tab だけを
attach し、選択外または background target の tab は background のまま保持する。

右ペインは session 名の右に tab を Chrome 風の chip として描き、その直下に active marker を置く。path は
右ペインには表示しない。chip の表示順・label は表示専用であり、選択は pending / document の `OperationId` または terminal live の完全な `TerminalRef` から投影する。
幅が狭い場合も ANSI を閉じた上で chip を clipping する。pending chip は v1 の選択 session と同じ
Nerd Font うさぎ `󰤇`（U+F0907）だけを
frame ごとに chip 内で進め、ラベル全体を着色しない。
tab が無い target は、灰色の静的うさぎと `No tabs stirring yet. Enter starts one.` の案内を、それぞれ
右ペイン幅の中央に表示する。描画前に clip して各灰色 SGR を reset で閉じるため、狭幅でも後続の
画面へ色が漏れない。この空状態は tick や runtime 接続に依存しない。overlay はこの Home frame を背景のまま合成する。

Closeup action modal の表示と input owner は target entry の tab 有無と forced action state から導く。Switch で
`Ctrl-O a` を実行した場合は、選択 target の Closeup action を開いて modal に input を渡す。tab が無い
Closeup は action modal が management input を所有し、Enter で `agent` / `terminal` を確定できる。tab が 1 つ以上で
forced state が無い Closeup は tab が input を所有し、action modal は自動表示しない。tab があるときに action modal
を再び出すのは `Ctrl-O a` だけで、その forced 表示は Esc で閉じて tab に戻る（Closeup から Switch へは抜けない）。
modal が所有する間、tab selection、close、terminal passthrough は dispatch しない。

Closeup action で `agent`、`terminal`、または `diff` を確定すると、同じ pending tab を即座に選択して右ペインへ
表示し、completion はその tab だけを live / document tab に置換して選択を維持する。diff は terminal identity を持たない
document tab として完了し、安全な document 本文を tab の content area に描画する。session の `terminal` は daemon が stable session / worktree scope を解決して起動する
`login-shell` であり、TUI はローカル PTY を生成しない。session が利用可能でない、または daemon が応答しない場合は
pending tab を安全な feedback に置き換える。`←` / `→`（または `h` / `l`）と `Ctrl-O n` / `Ctrl-O p` は tab を巡回し、`x` は
選択 tab を閉じる。close 後は次の tab（末尾なら直前）を stable identity で選択し、最後の tab を閉じたときだけ
target selection と Closeup action の空状態へ戻る。close は client-side selection を外すだけであり、daemon-owned
terminal を停止しない。

各起動 request は launch / document resolve の前に一度描画されるため、pending chip は既存の共有 shimmer wave を
必ず表示する。completion が到着した後の次フレームでだけ、同じ stable identity の live / document tab を選択する。

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

## Closeup Agent の手動確認

Agent profile を利用できる daemon を起動し、既存 session を選択して Closeup を開く。次の操作は実装済みの
runtime bridge を確認する手順である。profile の install 状態、認証内容、argv は画面に入力・表示しない。

| 操作 | 確認結果 |
| --- | --- |
| Action menu の Agent、または `agent codex` を確定する | 同じ session の `Agent (starting)` tab が出て、daemon が受理した operation として pending のまま表示される |
| matching final を daemon が replay する | pending が Agent tab に一度だけ置換され、選択中なら attach される |
| Agent が stdout を出力する | 選択中 Agent tab の pane に出力が表示される |
| 選択中 Agent tab で入力し、端末を resize する | 入力は一度だけ daemon に届き、geometry が変わったときだけ resize が届く |
| daemon を切断して再接続する | process を作り直さず、inventory で検証済みの選択 tab だけが attach/resync される |
| profile 未準備・daemon 不通・Agent exit を発生させる | pending/tab state は収束し、安全な inline feedback だけが表示される |

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

`q` は確認後に TUI だけを閉じ、daemon-owned terminal は継続する。`Ctrl-Q` は確認後に workspace の
live session すべてへ強制終了を要求してから TUI を閉じる。確認 modal は `[ ok ] [cancel]` を表示し、
`Enter`、左右、Tab、`o`、`c`、Esc で操作できる。
