# 提案: v2 TUI の v1 UI/UX parity 受け入れ契約

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [daemon lifecycle](05-daemon-lifecycle.md)

## 目次

- [要旨](#要旨)
- [正本と対象範囲](#正本と対象範囲)
- [優先度](#優先度)
- [Surface parity 表](#surface-parity-表)
- [Home の操作モデル](#home-の操作モデル)
- [MVP 受け入れ契約](#mvp-受け入れ契約)
- [ライブ端末の入力契約](#ライブ端末の入力契約)
- [daemon と backend の外部 checkpoint](#daemon-と-backend-の外部-checkpoint)
- [v2 の gap と再利用方針](#v2-の-gap-と再利用方針)
- [後回し項目](#後回し項目)
- [完了判定と正本への畳み込み](#完了判定と正本への畳み込み)

## 要旨

本提案は、v2 TUI を v1 と同様の操作モデルで日常利用できるようにするための、**実装前の parity
scope・優先度・受け入れ契約の正本**である。見た目の完全複製ではなく、workspace を開き、session を選び、
agent / terminal を操作し、TUI を閉じても安全に再接続できることを MVP とする。

TUI-01（[#222](../../.usagi/issues/222-docs-tui-v2-tui-v1-ui-ux-parity.md)）は本契約をレビュー可能にする
docs-only task であり、完了しても parity 実装が完了したことにはならない。後続 issue は本書の acceptance ID を
参照し、挙動を本文へ複製しない。

issue store は work breakdown・status・dependency の正本である。本書は「何を満たせば parity か」、issue は
「誰がどの単位をいつ実装するか」を所有する。

daemon / core / TUI の Rust 実装と IPC wire の変更は本提案の対象外である。daemon は別セッションが担当し、
本書では TUI が待ち合わせる外部 checkpoint だけを定義する。

## 正本と対象範囲

調査資料の優先順位は次のとおりとする。同じ事実が食い違う場合は上の行を採る。

| 優先 | 資料 | 用途 |
|---|---|---|
| 1 | v1 の現行コードとテスト。入口は [app/event.rs](../../v1/src/presentation/tui/app/event.rs)、Home の mode は [state/mode.rs](../../v1/src/presentation/tui/home/state/mode.rs)、一覧は [state/list.rs](../../v1/src/presentation/tui/home/state/list.rs)、入力は [pane_input.rs](../../v1/src/presentation/tui/home/pane_input.rs)、描画は [io/screen.rs](../../v1/src/presentation/tui/io/screen.rs)。代表 test は [session_lifecycle.rs](../../v1/src/presentation/tui/home/event/tests/session_lifecycle.rs)、[attached.rs](../../v1/src/presentation/tui/home/event/tests/attached.rs)、[quit_modal.rs](../../v1/src/presentation/tui/home/event/tests/quit_modal.rs) | v1 の現在挙動 |
| 2 | [Splash](../../v1/document/design/00-splash.md) から [Config](../../v1/document/design/04-config.md)、Home の [01-modes.md](../../v1/document/design/home/01-modes.md) から [05-overlays.md](../../v1/document/design/home/05-overlays.md) | コードと一致する範囲の意図と用語 |
| 3 | v2 の現行コードとテスト。[presentation/mod.rs](../../crates/tui/src/presentation/mod.rs)、[workspace.rs](../../crates/tui/src/presentation/views/workspace.rs)、[application.rs](../../crates/tui/src/usecase/application.rs)、[typed ID](../../crates/core/src/domain/id/mod.rs)、[src/main.rs](../../src/main.rs) | 移植開始点と gap |

古いトップ文書、画像、退避前の呼称は受け入れ根拠にしない。調査で確認した主なドリフトは次のとおりである。

- Home の基底 `Switch` で `Esc` は no-op であり、Open へ戻らない。
- Open の Unite 選択履歴は実イベント経路では復元されない。
- 表示幅は全角 CJK を 2 桁、East Asian Ambiguous を 1 桁として扱う。
- Splash は移動するうさぎではなく、固定うさぎとタイトルの fade である。

本書の「v2 の現在挙動」は TUI-01 の調査時点の baseline であり、実装済み仕様ではない。未実装事項を
`document/` 直下の仕様へ追記せず、本提案に留める。

## 優先度

| 優先度 | 意味 | リリース判定 |
|---|---|---|
| **A** | v1 と同様に workspace / session / terminal / agent を使うため必須 | 全件が MVP gate |
| **B** | MVP 後の機能 parity。代替経路はあるが、v1 の機能を欠く | MVP を阻害しない |
| **C** | 装飾・pixel parity・厳密な演出 | 機能リリースを阻害しない |

## Surface parity 表

| Surface | v1 の現在挙動 | v2 の現在挙動 | parity 方針 | 優先度 | acceptance test | daemon / backend 依存 |
|---|---|---|---|---|---|---|
| Splash | 起動時に 1 回、Welcome と同位置の固定うさぎへタイトルを fade する。入力を読まず type-ahead を残す | surface / runtime が無く、直ちに Welcome を描く | MVP では省略可。追加時も入力を奪わず、Welcome との layout shift を起こさない | C | `C-SPLASH-1`: 再生中の `o` / `q` が次の Welcome で 1 回だけ処理される | なし |
| Welcome | Open / New / Config / Quit、Recent 最大 3 件。矢印 / `j k`、Enter、文字 shortcut、`1..3`、安全な終了 | 同じ 4 項目と Recent view、矢印 / Enter / shortcut、単体 Recent→Workspace は接続済み。Unite Recent は表示のみ | Open / 単体 Recent→Home と端末復元を A。New / Config の実処理を B、完全なカード装飾を C | A / B / C | `A-ENTRY-1`: Open と Recent の両経路から Home を開き、quit 後に端末属性を復元する | Home attach は `D1` |
| Open | workspace 名で filter し Single / Unite を選択。欠損登録の削除確認、統計、確定後の演出を持つ | 単一一覧の名前・相対時刻・path、矢印 / Enter / Esc / q。filter / Unite / 欠損 cleanup は無い | Single の選択と Home 遷移を A。filter / cleanup / Unite を B、確定 animation を C | A / B / C | `A-ENTRY-2`: 2 件から選択した stable identity の Home が開く。空一覧 Enter は no-op | registry は core、Home は `D1` |
| New | Clone / Existing、文字単位 caret、directory picker、validation、clone / register、失敗時の form 保持、成功後 Home | Clone / Existing の純粋 view と基本編集、自動導出はある。Enter は no-op で backend 未接続 | 現行 view / state tests を保ち、両方式の成功→Home と再試行可能な error を B で接続する | B | `B-NEW-1`: Clone / Existing の成功は Home、失敗は全入力を保った New | project / git / registry backend |
| Config | global / workspace scope、dirty 表示、明示 Save、各種 chooser / editor / 非同期 probe | マスコットと `No settings` の placeholder。Esc と quit 以外は no-op | global / workspace scope の read-edit-save を B。Local LLM 導入は後回し | B | `B-CONFIG-1`: scope ごとの変更だけを保存し、失敗時は編集値と error を保つ | settings backend |
| Home | root / session / `+ new`、selected / active、Switch / Closeup、live pane、phase、非同期 lifecycle を統合 | Switch / Closeup と一層の overlay 合成は接続済み。session の後ろに root、単一 selected、target 共通の固定 `Preview/Terminal/Diff/Notes` を持ち、`+ new` / active / live pane / phase は無い | 既存 mode / 2-pane / overlay runtime を足場に parity state / event runtime へ拡張する | A | [MVP 受け入れ契約](#mvp-受け入れ契約)の全 A ID | `D1`〜`D6` |
| overlay: Overview | `:` で Home に重なる Workspace scope の command surface。補完、履歴、結果 / error を持つ | Switch / Closeup の上へ背景を残して開閉・入力できる。候補は hard-coded dummy で Enter は no-op | registry dispatch、result / error を A。Tab completion / history recall を B とし、mode を増やさない | A / B | `A-MODE-1` / `A-DISPATCH-1` | session 操作は `D2` |
| overlay: Closeup | 対象 session 上に浮く Menu / Prompt。terminal / agent / close / diff を実行する | Closeup mode の既定 overlay として背景上へ合成され、選択できる。action は hard-coded dummy で Enter は no-op | Session scope の action effect を接続し、live terminal を同じ Closeup 内に保つ | A | `A-MODE-1` / `A-PANE-1` | `D2`〜`D5` |
| overlay: progress / error / quit | create / remove skeleton、operation error、quit confirmation。入力中も更新する | loading widget はあるが未使用。基底の q と全状態の Ctrl-C は即時 quit、loader error は runtime を終了 | pending / failure / reconnect と detach-safe quit を A。固定 layout の既存 widget を再利用する | A | `A-LIFE-1` / `A-FEEDBACK-1` / `A-QUIT-1` | `D2` / `D6` |
| overlay: diff / note / PR / text / env | diff、note / todos / decisions、PR、長文、env editor などを Home に合成する | PR は選択 session の実 model で開閉・移動できるが Enter は no-op。diff / note / text / env view は未実装で、Home の Diff / Notes は label / path placeholder だけ | コア操作を塞がない独立 B issue とし、既存 overlay 合成 / renderer を共用する | B | `B-OVERLAY-1`: 背景 state を失わず開閉し、長文は安全に scroll する | 機能ごとの backend |

## Home の操作モデル

Home のトップレベル mode は **Switch / Closeup の 2 つだけ**である。

| surface | scope | 所有する操作 |
|---|---|---|
| Switch mode | Session set | root / session / `+ new` の選択、session 作成、active target の切替 |
| Closeup mode | Selected target | tab / pane の選択、Closeup modal、preview、live terminal |
| Overview modal | Workspace | `session` / `issue` / `config` / `env` など workspace 全体の command |
| Closeup modal | Session | `terminal` / `agent` / `close` / `diff` など 1 target の action |
| live terminal | Closeup の内部状態 | daemon 所有 pane の画面、入力 passthrough、予約キー |

`Overview` を常駐 mode にせず、live terminal を第 3 mode にしない。状態遷移は次を正本とする。

```text
attach（保存済み参照無し）────────────────────────────▶ Switch
  │
  ├─ : ───────────────────────────────────────────────▶ Overview modal
  │                                                     │ Esc
  │                                                     ▼
  ├─ Enter（非 live）/ t / Ctrl-O a ─────────────────▶ Closeup / Closeup modal
  └─ Enter（live）────────────────────────────────────▶ Closeup / live terminal
                                                        │
                           Ctrl-O a ─────────────────────┤ modal を重ねる
                           Ctrl-O o ─────────────────────┤ Switch
                           detach → client 再起動 → attach ┘ 同じ terminal を reattach
```

Switch 基底の `Esc` は no-op である。終了は `Ctrl-C` / `Ctrl-Q` の安全契約を通す。Switch から入った
Closeup modal / preview の `Esc` は Switch へ戻る。live pane から `Ctrl-O a` で浮かせた Closeup modal の
最初の `Esc` は元 pane へ reattach する。その後の `Esc` は terminal input なので PTY へ流れる。

### 一覧の identity

一覧は workspace ごとに次の順序を持つ。

```text
⌂ root
session 1
session 2
…
+ new session
```

| 要素 | 契約 |
|---|---|
| root | session ではない常設 target。terminal / agent の cwd は workspace root |
| session | daemon snapshot の stable session identity で追跡する。表示名や並び順を identity にしない |
| `+ new session` | 常設 action row。active target にはならず、pending create 中も残る。Enter または printable key で inline name input を始める |
| selected | TUI の navigation cursor。移動は preview だけを変える |
| active | 後続 command / Closeup の target。Enter / 明示 switch でだけ selected に追従する |

TUI 内の target key は `Root(WorkspaceId) | Session(SessionId)` という **view projection** であり、wire 型を
新設しない。effect は [IPC／ID の typed resource](02-ipc-id.md#resource-graph)へ変換し、root terminal は
`SessionId = null` として扱う。`+ new` は target に含めない。本書の「Session scope」は workspace 全体でなく、
この 1 target に閉じた操作 scope を意味する。root では terminal / agent を許し、session 専用の close / diff は出さない。

保存済み参照の無い初回 attach では selected / active とも root から始める。両者は別状態であり、cursor 移動だけで実行対象や
live pane を切り替えない。selected の cursor と active の gutter を独立して描き、別の行を指す状態でも両方を
同時に識別できるようにする。snapshot 更新時は配列 index ではなく stable identity で両者を復元する。通常 refresh で
identity が消えた場合は同じ workspace の root へ戻し、明示 remove 後の landing だけを `A-LIFE-2` の例外とする。

## MVP 受け入れ契約

以下の A test をすべて自動化し、表の test level に従って suite 全体で fake backend と実 PTY の両境界を
覆ったときだけ MVP 完了とする。

| ID | Given / When / Then | 主な test level | 外部 checkpoint |
|---|---|---|---|
| `A-ENTRY-1` | Welcome→Open→Home と Welcome→Recent→Home の両経路で、同じ alternate screen 上に対象 Home が描かれる。終了時は raw mode、cursor、mouse、alternate screen を必ず復元する | runtime + PTY | `D1` |
| `A-ENTRY-2` | Open の Single 一覧で選択した workspace と Home snapshot の identity が一致する。空一覧 / stale response / open error は別 workspace を開かず、画面内 error から再試行できる | runtime + fake backend | `D1` |
| `A-HOME-1` | 0 件 / 複数件で root→sessions→`+ new` を描き、保存済み参照無しなら selected / active は root とする。移動は active を変えず別 marker を同時表示し、`+ new` は active にならない。root pane の cwd、refresh / remove fallback も typed identity から決まる | pure model + render | `D1` |
| `A-LIFE-1` | accepted 後も event loop を止めない。create は実配置に非選択 skeleton、remove は既存 selectable row を selected / active marker ごと in-place skeleton 化する。create failure は skeleton を外して `+ new`、remove failure は元 row を戻す。現在地を巻き戻さず safe error を残し、event は `OperationId` で対応付けて外部正本の revision / sequence で stale / duplicate を捨てる | reducer + fake daemon | `D2` |
| `A-LIFE-2` | accepted 時の TUI-local interaction counter を記録し、inert を含む全 key / click / scroll / right-click input で増分する。**成功** completion 時も同値なら create 後は新 session の Closeup、remove 後は隣 session（無ければ root）の Switch へ移る。failure、操作済み、live 中は現在地を奪わない | reducer | `D2` |
| `A-PANE-1` | terminal / agent tab を stable `TerminalRef` で混在表示する。`OperationId` に結び付く resolving / starting placeholder は success で live tab、failure で除去＋error になる。terminal exit は tab を除去して次 tab、最後なら Closeup へ戻る。live 時にも requested target / pending tab が選択中なら attach し、別 target / tab を選択済みなら background に残す。他の入力だけでは取消さない。TUI-local に保存した `TerminalRef` が有効なら client 再起動後も reattach できる | reducer + fake daemon + PTY | `D1` / `D3` / `D4` / `D6` |
| `A-MODE-1` | ladder は Switch / Closeup だけ。Switch Esc、`:` の origin return、non-live / live Enter、`t` / `Ctrl-O a`、live の `Ctrl-O o/a`、origin 別 Closeup Esc、last-pane exit の全遷移を固定する | transition table test | なし |
| `A-DISPATCH-1` | `+ new` の Enter / printable→validation→create を `D2` へ 1 回、Closeup terminal / agent→open / reuse を `D3` へ 1 回、Closeup close→remove を `D2` へ 1 回 dispatch する。不正入力、root の close、二重 Enter は request を出さない。Overview は registry の workspace command を実行し、success / safe error を結果帯へ出す | reducer + fake ports | `D2` / `D3` |
| `A-INPUT-1` | plain q / Esc / UTF-8 / 矢印・Home・End・Page / Ctrl-C / 通常 Alt / paste を正しい bytes と順序で 1 回だけ送る。Press / Repeat は各 1 event、Release は送らず、予約 action は PTY に届かない | pure encoder + PTY | `D4` |
| `A-INPUT-2` | `Ctrl-O` leader の timeout、double leader、`o/a/n/p/g/x/q` / 矢印、未知後続の 1 回 swallow、`Ctrl-^` が二重発火しない。timeout 後の次 key は fresh input として処理する | transition test + PTY | `D4` |
| `A-PHASE-1` | phase ready / running / waiting / ended / exited を該当 `AgentRuntimeRef` だけへ適用し、ended / exited を UI の done に畳む。target の複数 runtime は done > waiting > running > ready > absent で集約し、キー無しでも push / tick で更新する | reducer + fake daemon | `D5` |
| `A-FEEDBACK-1` | progress / terminal・operation error / disconnect を固定領域へ表示し、操作継続または再接続できる。error envelope の safe message と `error_id` を表示し、panic payload / secret / 内部 detail を画面へ出さない | reducer + render | `D2` / `D3` / `D6` |
| `A-QUIT-1` | live 中 Ctrl-C は PTY、management Ctrl-C は live 有りで確認・無しで即 detach、pane 離脱直後の最初の Ctrl-C は one-shot で吸収し他入力でも grace を解除する。Ctrl-Q は management で常に確認する。modal は Ctrl-C / Ctrl-Q が inert、`y/Y` / Enter で detach、`n/N` / Esc で取消し、daemon の terminal / operation を止めない | reducer + PTY | `D6` |
| `A-RENDER-1` | identical frame は content write 0、1 span 変更はその span だけ、短縮は stale suffix を消す。通常は row / column diff、surface reset は row repaint、resize は base を捨て full clear する。同 geometry の PTY resize は dedupe し、resize artifact で quit せず cursor / style を復元する。ANSI は幅 0、全角 CJK は 2、Ambiguous は 1、wide glyph を分断しない | pure renderer + PTY | resize は `D4` |

`A-LIFE-1` の remove は既定で dirty session を拒否し、破棄を伴う force は別の明示操作にする。
TUI を閉じても daemon が operation を完遂するため、v1 の「終了時に worker を join」は v2 へ移植しない。
再接続後の `D1` / `D2` snapshot が最終状態を回復する。画面内 open error の再試行と daemon 継続・reattach は、
v1 の process ownership をそのまま複製せず、v2 の安全性を強化する意図的差分である。

## ライブ端末の入力契約

ライブ terminal / agent では「予約されていない入力は daemon 所有 PTY へ渡す」を原則とする。plain `q`、
`Esc`、UTF-8、矢印、Home / End / Page、通常の Ctrl / Alt chord を TUI command に誤変換しない。

| 入力 | A の既定 prefix 方式 | TUI の動作 |
|---|---|---|
| 通常キー / paste | prefix 待ちでなければ passthrough | bytes と順序を保って `D4` へ送る |
| `Ctrl-O` | 最大 1 秒だけ次のキーを待つ leader | 単独では PTY へ送らない |
| `Ctrl-O o` / `Ctrl-O Ctrl-O` | 予約 | Switch |
| `Ctrl-O a` | 予約 | 同じ Closeup 内に Closeup modal |
| `Ctrl-O n/p` / `Ctrl-O ←/→` | 予約 | tab 切替 |
| `Ctrl-O g` | 予約 | agent pane の追加または既存 pane へ reattach |
| `Ctrl-O x` | 予約 | active tab を close |
| `Ctrl-O q` | 予約 | quit confirmation |
| `Ctrl-^` | 予約 | 直前の session。無ければ no-op |
| `Ctrl-C` | 予約しない | text selection を後回しにする A では terminal / agent へ interrupt として渡す |
| `Ctrl-Q` | 予約しない | live 中は PTY へ渡す。management では常に quit confirmation |
| 未知の leader 後続キー | 予約 | leader と後続キーを 1 回だけ読み捨て、二重発火しない |

leader timeout では leader 自体だけを読み捨て、次の key は fresh input として処理する。modifier、key kind、
text / raw bytes を失わない入力語彙が必要である。Alt 方式、`Ctrl-O e` の note、`Ctrl-O s` の sidebar toggle、
tab reorder、text selection 中 Ctrl-C の copy は B とし、A の prefix passthrough を先に固定する。

## daemon と backend の外部 checkpoint

現在の IPC transport は [core IPC](../../crates/core/src/infrastructure/ipc/mod.rs) と
[daemon handler](../../crates/daemon/src/presentation/ipc.rs) の Ping / Pong 語彙・一接続の逐次 handler までで、実 socket
server / TUI client へ未合成である。一方、typed resource ID、aggregate reference、pure fencing は
[core domain ID](../../crates/core/src/domain/id/mod.rs) に実装済みである。wire / ID / daemon lifecycle の正本は [ID 契約](02-ipc-id.md)、
[IPC protocol](03-ipc-protocol.md)、[daemon API](04-daemon-api.md)、[daemon lifecycle](05-daemon-lifecycle.md) とする。
次の `D1`〜`D6` は TUI acceptance から外部正本への**結合 checkpoint alias**であり、wire の型名、field、順序軸、
error shape をこの文書では再定義しない。

| ID | 外部 contract の正本 | TUI 側の受け入れ checkpoint |
|---|---|---|
| `D1 WorkspaceAttach` | [resource graph](02-ipc-id.md#resource-graph)、[subscribe barrier](03-ipc-protocol.md#subscribe-barrier)、[session command surface](04-daemon-api.md#command-surface) | `WorkspaceId`、typed session / worktree identity、`state_revision` を snapshot / replay の barrier から一貫して得る。TUI-local に保存した target は fresh workspace snapshot に存在するときだけ復元し、無効なら root の Switch へ縮退する |
| `D2 SessionLifecycle` | [operation API](03-ipc-protocol.md#operation-api)、[session command surface](04-daemon-api.md#command-surface) | create / remove の `OperationId` を pending row と対応付け、accepted / progress / final を reducer へ流す。disconnect 後は `OperationList` / subscribe と workspace snapshot で reconcile し、同じ durable intent を二重実行しない |
| `D3 TerminalInventory` | [aggregate reference](02-ipc-id.md#aggregate-reference)、[terminal API](04-daemon-api.md#terminal-api) | pending tab は `OperationId`、live tab は `TerminalRef`、agent は `AgentRuntimeId` / `AgentRuntimeRef` で追跡する。`TerminalList` / attach と spawn / kill operation から open / reuse / close を合成し、保存した terminal が無ければ Closeup へ縮退する |
| `D4 TerminalStream` | [ordering・revision・resume](03-ipc-protocol.md#orderingrevisionresume)、[terminal API](04-daemon-api.md#terminal-api) | retained replay が安全なら継続し、gap、epoch 不一致、parser state を証明できない場合だけ snapshot で置換する。input は ACK の full / partial-ambiguous / failed を区別して blind retry せず、resize は geometry change 時だけ送り、detach で PTY を kill しない |
| `D5 AgentPhase` | [状態軸を分ける](04-daemon-api.md#状態軸を分ける)、[phase ingestion](04-daemon-api.md#phase-ingestion) | `AgentRuntimeRef` ごとの phase projection を更新し、ended / exited を UI の done へ畳む。producer、resource、delivery の順序軸は外部正本へ委譲し、terminal exit による tab 除去とは混同しない |
| `D6 ConnectionSafety` | [bootstrap handshake](03-ipc-protocol.md#bootstrap-handshake)、[disconnect と timeout](03-ipc-protocol.md#disconnect-と-timeout)、[error envelope](03-ipc-protocol.md#error-envelope)、[daemon lifecycle](05-daemon-lifecycle.md#daemon-lifecyclerestartcrash) | version mismatch、disconnect、reconnect、resync を typed state として表示する。TUI quit は client detach だけとする。client / TUI 再起動と planned rollover は有効な `TerminalRef` へ再接続し、daemon crash は fresh process で代替せず orphan / error / reconcile state を表示する |

session の表示順は `WorkspaceSnapshot` 側で deterministic order または order key を一度だけ定義する必要がある未解決 checkpoint である。
TUI は UUID timestamp から順序を推測しない。error 表示は [error envelope](03-ipc-protocol.md#error-envelope) の safe
message と `error_id` を使い、secret や内部 detail を画面へ出さない。

責務の境界は次のとおりである。

| TUI が所有 | daemon が所有 | core / その他 backend が所有 |
|---|---|---|
| runtime 中の selected / active、Switch / Closeup、modal、pane / tab / focus、interaction counter、skeleton の配置、frame diff、任意の local resume state | session mutation の直列化、terminal / PTY / child、operation、Agent runtime と各 snapshot / event の権威 | workspace registry、New の clone / register port、Config の settings port |

wire / daemon API が未実装でも、TUI は fake daemon で mode、modal、selected / active、pending / error、reserved key、renderer を
先行実装できる。実 terminal passthrough、operation 完了、phase、reattach の結合だけを checkpoint 待ちにする。

## v2 の gap と再利用方針

### 現在の主な gap

| gap | コード根拠 | 影響 | parity 方針 |
|---|---|---|---|
| attach transport 未実装 | [core IPC](../../crates/core/src/infrastructure/ipc/mod.rs) / [daemon handler](../../crates/daemon/src/presentation/ipc.rs) は Ping / Pong と一接続の逐次処理まで。[typed ID / fencing](../../crates/core/src/domain/id/mod.rs) は実装済みだが、[TUI infrastructure](../../crates/tui/src/infrastructure/mod.rs) は説明だけ | socket accept、subscribe、snapshot / event client が無い | 実装済み typed ID を使って `D1`〜`D6` を fake port で先行し、daemon session の transport と後で結合 |
| blocking `read_key` | [application.rs](../../crates/tui/src/usecase/application.rs) の `Terminal::read_key` と [presentation/mod.rs](../../crates/tui/src/presentation/mod.rs) の同期 loop | キーが無い間に daemon push、progress、phase、animation を処理できない | key / resize / daemon / tick を 1 event stream として reducer へ渡す |
| modifier / key kind 欠落 | [application.rs](../../crates/tui/src/usecase/application.rs) の `Key` と [src/main.rs](../../src/main.rs) の crossterm 変換 | Ctrl-C は常に Quit、Ctrl / Alt / Shift を失い、Repeat、Tab / Home / End / Delete、Paste を捨てる | modifier、kind、text / bytes を保持する input event に置換 |
| Workspace placeholder | [workspace.rs](../../crates/tui/src/presentation/views/workspace.rs) の mode と固定 4 tab / path 本文 | Switch / Closeup はあるが root が末尾で、typed target projection、`+ new` / active / pane / phase が無い。active tab index も全 target 共通で、Switch Esc は Home を戻ってしまう | 既存 mode、2-pane layout、viewport を parity state / transition へ接続 |
| modal effect 未接続 | [presentation/mod.rs](../../crates/tui/src/presentation/mod.rs) は Overview / Closeup / PR の開閉と背景合成を行うが、各 Enter action は no-op | TUI-01 着手時の「modal 未接続」は開閉・合成まで解消したが、command / browser / pane effect を実行できない | 既存 overlay 合成を `A-DISPATCH-1` の registry / effect port に接続 |
| dummy registry / `NotImplemented` | Overview の [view](../../crates/tui/src/presentation/views/overview_modal.rs) と [usecase](../../crates/tui/src/usecase/overview/mod.rs)、Closeup の [view](../../crates/tui/src/presentation/views/closeup_modal.rs) と [usecase](../../crates/tui/src/usecase/closeup/mod.rs) が別候補を持ち、全 handler が stub | 表示候補と実行可能 command が既に不一致 | usecase registry を metadata / completion / dispatch の SSoT にし、effect port を注入 |
| full clear 描画 | [src/main.rs](../../src/main.rs) の `draw` は毎 frame `Clear(All)` | flicker、帯域増、live terminal の差分を潰す | previous frame / cell grid を保持し、`A-RENDER-1` を満たす |
| error / progress UI 未接続 | [presentation/mod.rs](../../crates/tui/src/presentation/mod.rs) は loader error を返し、[loading.rs](../../crates/tui/src/presentation/widgets/loading.rs) は runtime 未使用 | 再試行できず TUI が閉じる | 既存 Welcome / New notice slot と loading widget を typed event、skeleton、reconnect state へ接続 |

### 再利用する実装と test

| 足場 | 再利用方針 |
|---|---|
| [welcome.rs](../../crates/tui/src/presentation/views/welcome.rs)、[open.rs](../../crates/tui/src/presentation/views/open.rs)、[new.rs](../../crates/tui/src/presentation/views/new.rs)、[config.rs](../../crates/tui/src/presentation/views/config.rs) | geometry / style / state primitive と幅・選択・notice tests を優先再利用し、pure state と backend effect を parity 契約へ拡張する |
| [workspace.rs](../../crates/tui/src/presentation/views/workspace.rs) と [panes.rs](../../crates/tui/src/presentation/layouts/panes.rs) | 2-pane geometry、viewport、domain record adapter、tiny-size tests と `mode_transitions_preserve_the_session_and_tab_selection` / `focused_label_and_pull_requests_follow_the_selected_session` を残し、hard-coded tab state を置換する |
| [modal.rs](../../crates/tui/src/presentation/widgets/modal.rs) と 3 modal view | `boxed` / `modal_inner_width` / `render_over`、filter、selection、ANSI / CJK / tiny terminal、背景を保つ合成 tests を保持し、dummy action だけを実 registry / effect へ置換する |
| [widgets/mod.rs](../../crates/tui/src/presentation/widgets/mod.rs) と [text_input.rs](../../crates/tui/src/presentation/widgets/text_input.rs) | ANSI 幅 0、CJK 表示幅、Unicode scalar（`char`）境界の clip / edit / caret tests を `A-RENDER-1` へ昇格し、Ambiguous=1 fixture を足す |
| [loading.rs](../../crates/tui/src/presentation/widgets/loading.rs) | progress / rabbit の純粋 widget を create / remove / pane starting の固定領域へ流用する |
| [presentation/mod.rs](../../crates/tui/src/presentation/mod.rs) の `FakeTerminal` / `FakeLoader` tests | `modal_reducers_capture_edit_selection_and_close_keys` / `switch_pr_modal_captures_keys_without_moving_the_background` / `workspace_modes_modals_tabs_and_escape_stack_are_interactive` を足場に fake daemon event queue を追加し、画面 graph と reducer の integration test に拡張する |
| [cli_tui_pty.rs](../../tests/cli_tui_pty.rs) | alternate screen 復元 test を残し、resize、passthrough、reserved key、detach / reattach の PTY regression を追加する |

既存 pure view の geometry / style / state primitive と characterization test を優先再利用する。placeholder / dummy /
hard-coded state と runtime 非対応 API は parity model へ置換し、その期待値も本受け入れ契約へ更新する。
現行 Switch Esc と即時 quit の test は現状把握用 characterization に留め、parity の期待値としては流用しない。

## 後回し項目

次は A MVP を完了させずに先行実装しない。

| 優先度 | 項目 |
|---|---|
| B | New の完全な clone / Existing / directory picker、Config の global / workspace editor |
| B | Open の filter、欠損 registry cleanup、**Unite**、複数 workspace sidebar |
| B | Overview の Tab completion / history recall、command help / long text |
| B | note / todos / decisions、rich diff、PR / text / env modal、bulk remove checklist |
| B | local LLM / chat / install、wake、self-update、native terminal、Alt key scheme、sidebar toggle |
| B | note chord、mouse click、text selection / Ctrl-C copy、tab reorder / **mouse drag**、scrollback の高度な操作 |
| C | Splash、Open の着地演出、exact animation、AA、color、wave、Nerd Font **glyph** |

上表を MVP 外項目の正本とする。後続 issue はこの表へリンクし、現在のビルド仕様へ予定として記載しない。

## 完了判定と正本への畳み込み

- TUI-01 の完了条件は、本提案・目次・検証結果を同じ PR でレビュー可能にすることである。
- parity MVP の完了条件は、[MVP 受け入れ契約](#mvp-受け入れ契約)表の全 A ID と `D1`〜`D6` の結合を満たすことである。
- B / C は MVP 完了を阻害しない。各後続 issue は acceptance ID 単位に分割する。
- 実装済みになった挙動だけを `document/` 直下の画面仕様へ畳み込み、対応する本書の current / gap 記述を削る。
- 全 A/B/C が仕様へ畳み込まれた時点で、本提案はリンク stub 化または撤去する。
