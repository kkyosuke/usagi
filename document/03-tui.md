# 3. TUI

> [ドキュメント目次](README.md) ｜ ← 前へ [2. アーキテクチャ](02-architecture.md) ｜ 次へ → [4. daemon IPC](04-ipc.md)

v2 TUI の現在の画面遷移、live pane、および TUI-local resume state の仕様である。daemon
の resource schema や wire protocol は本書では所有せず、[4. daemon IPC](04-ipc.md) と
[5. daemon](05-daemon.md) を境界の正本とする。

## 目次

- [画面と入力](#画面と入力)
- [workspace の離脱と終了](#workspace-の離脱と終了)
- [settings scope と workspace entry](#settings-scope-と-workspace-entry)
- [workspace の選択と daemon](#workspace-の選択と-daemon)
- [Home と target](#home-と-target)
  - [Switch の右ペインは cursor の preview](#switch-の右ペインは-cursor-の-preview)
- [指示モード（Director mode）](#指示モードdirector-mode)
- [Home frame loop と背景観測 lane](#home-frame-loop-と背景観測-lane)
- [frame 予算](#frame-予算)
- [Session sidebar rows](#session-sidebar-rows)
- [Overview と modal](#overview-と-modal)
- [session garden](#session-garden)
- [PR modal と browser effect](#pr-modal-と-browser-effect)
- [Sidebar mascot](#sidebar-mascot)
  - [daemon health indicator](#daemon-health-indicator)
  - [session 状態別件数](#session-状態別件数)
  - [Agent concurrency](#agent-concurrency)
- [Closeup pane](#closeup-pane)
- [Closeup の agent CLI 選択](#closeup-の-agent-cli-選択)
- [Closeup 入力の拒否表示](#closeup-入力の拒否表示)
- [Closeup Agent の手動確認](#closeup-agent-の手動確認)
- [workspace open 時の pane 復元](#workspace-open-時の-pane-復元)
- [resume data compatibility](#resume-data-compatibility)
- [exited terminal の completed entry](#exited-terminal-の-completed-entry)
- [interrupted Agent の tab 投影と明示 resume](#interrupted-agent-の-tab-投影と明示-resume)
- [feedback と終了](#feedback-と終了)

## 画面と入力

Welcome は Open / Recent / New / Config の入口である。Open は登録済み workspace を名前の
大文字・小文字を区別しない alphabet 順に並べる。常時表示する Filter 欄は編集位置に cursor を
示し、入力した文字で即座に名前を絞り込み、↑↓ で絞り込み結果を選ぶ。各 workspace は名前と、session 数・未完了 issue 数・
最終更新の相対時刻を 2 行で表示する。Recent は上下の内側余白を持たない compact card で表示し、
同じ Workspace 画面を直接開く。New と Config は
それぞれの backend port を通じて作成・保存し、失敗時は入力中の draft を保持する。

New は Clone（リポジトリを新しいディレクトリへ clone）と Existing（既存ディレクトリを登録）の
2 モードを持ち、`←→` でモードを切り替え、`↑↓`/Tab でフィールドを移動する。必須項目が揃った状態で
`Enter` を押すと作成を実行する。必須項目が欠けているときの `Enter` は、最初に不足しているフィールドの
安全なメッセージを notice に出して同画面に留まり、入力は保持する。

`Enter` は作成の副作用（ディレクトリ作成・`git clone`・registry への登録）に進む前に事前検証し、
弾いた場合は何も作らないまま同画面に留まって draft を保持する。したがって入力を直してそのまま再実行できる。
事前検証する条件は次のとおりで、いずれも安全で具体的な 1 行メッセージを notice に出す。

- **入力された workspace がすでに存在する**: 既に登録済みの path、または Clone の宛先ディレクトリが
  既に存在する場合は、作成へ進まずエラーを表示する（同一性は path の完全一致で判定する）。
- **不正なパス**: Clone の directory 名にパス区切り（`/` `\`）や `.` / `..` が含まれる、Existing の
  path が存在しない・ディレクトリでない場合はエラーにする。
- 前後の空白は判定前に trim する。判定は正規化せず完全一致で行うため決定的で、Unicode を含む名前も
  不当に弾かない。

作成は token と検証済み request identity を持つ effect として、entry loop の外にある単一 worker へ dispatch する。
実行中も entry loop は tick・描画・入力・resize を処理し、New は spinner を更新する。同時に実行する作成は 1 件で、
実行中の編集と `Enter` は無視する。completion は token と request の両方が pending operation と一致するときだけ採用し、
stale / duplicate completion が別の draft や workspace を開くことはない。

作成が成功すると、その workspace を Open / Recent と同じ snapshot/composition 経路で 1 回だけ Home へ遷移する。
作成が失敗すると、入力中の draft を保ったまま安全な notice を出して同画面に留まり、そのまま修正・再実行できる。
実行中の `Esc` は操作の画面上の待機を cancel して Welcome へ戻り、completion は workspace を開かず破棄する。
worker 内の `git clone` 自体は強制終了しないため、処理が完了するまで新しい作成は開始しない。失敗・cancel のどちらでも
既存 directory は削除せず、clone が途中まで作った destination も自動削除しない。`Ctrl+C` / `Ctrl+Q` は TUI を終了する。

Welcome の Config は、`Global` 見出しに全体へ即時適用する Theme・Modal mode・Environment、`Workspace init` 見出しに
新規 workspace の初期値となる Agent・Issue・Memory を表示する。開いている workspace の Overview で `config` を
実行した場合は、Home 上の overlay modal に Agent・Issue・Memory だけを表示し、scope 表示は行わない。どちらも
`↑↓` で行を、`←→` で値を切り替える。未保存の値には `●` が付く。dirty な Save 行で `Enter` を押すと保存フローが始まり、
Save button 自体が **loading（`saving…`）** 表示に変わる。保存が成功すると同じ button が **`saved`** 表示へ変わり、
短い確認表示ののち、ユーザー操作なしで呼び出し元へ自動的に戻る。Welcome の Config は Welcome へ戻る。
Overview の Config は、その workspace を settings port に束縛し、live pane と session を背景に維持したまま Home へ戻る。
保存が失敗した場合は
自動で戻らず Config に留まり、`Save failed: …` の notice を出す。draft は dirty のまま保たれるため、
その場で確認・修正して再試行できる。
保存の実行中は入力を読まず、保存中の再押下（連打）は無視されるため、保存が二重に走ることはない。`Esc` は
呼び出し元へ戻る。Welcome または `usagi config` から開いた全画面 Config では `Ctrl+C` / `Ctrl+Q` で終了する。
Workspace 上の overlay modal では両キーを消費して Config に留まり、背面の Home へ終了操作を伝播しない。

フォーカス中で編集可能な 1 行入力は共通の block cursor を使う。挿入位置の Unicode scalar を
入力値と同じ意味色の reverse-video で示し、空欄または行末では反転した空白 1 セルを示す。
この表示は文字を横へ押し出さず、全角文字も 1 scalar 単位で扱う。非フォーカス値、読み取り専用値、
候補・選択行の強調はそれぞれの既存表示を維持する。

編集可能な 1 行入力（New フォーム・Open の Filter・Overview / Closeup palette など、共通入力
widget を使う各欄）は、キャレット移動・範囲選択を一貫して扱う。`←`/`→` で 1 文字ずつ、`Home` で
行頭・`End` で行末へキャレットを移す。テキスト入力にフォーカスがある間は emacs の `Ctrl-A` が行頭・
`Ctrl-E` が行末で、`Ctrl-E` は `End` と等価である。`Shift`+`←`/`→` はキャレットから 1 文字ずつ選択を
広げ、`Shift`+`Home`/`End` は行頭 / 行末まで一括選択する。選択中に文字を打つとその文字へ置換し、
`Backspace`/`Delete` は選択範囲をまとめて消してキャレットを削除位置へ置く。`Shift` を伴わない移動
（`←`/`→`/`Home`/`End`）と `Esc` は選択を解除する。選択・置換・削除はいずれも scalar 境界に乗り、
CJK / 全角を含んでもハイライト幅が見た目とずれない。

`Ctrl-A` / `Home` は文脈依存である。上記のとおり編集可能入力にフォーカスがある間はキャレットを行頭へ
移すが、フォーカスの無い Home の navigation（Switch）では従来どおり `+ new session` を開く（[Home
management input](#home-と-target) 参照）。この境界は「テキスト入力にフォーカスがあるか」で切り分け、
両者の意味が衝突しない。

Home を開く入口は direct workspace、Welcome の Recent、Open の選択、New の作成成功で共通である。
いずれも workspace snapshot を同じ production backend factory に渡し、factory が生成した
`DaemonBackend` と同一の port set を使う。Home controller が発行した Effect は
`DaemonBackend::dispatch` だけが解釈し、session / Agent / terminal、notes / environment、workspace command、
decision、PR snapshot / preview、browser、desktop notification へ振り分ける。別の screen-graph executor や
production fallback stub は持たない。

**Home からは Welcome へ戻れる**。入口は片道ではなく、workspace を離れて別の workspace を開くために
プロセスを終了する必要はない。離脱と終了の区別、および離脱時の teardown は
[workspace の離脱と終了](#workspace-の離脱と終了)を正本とする。

## workspace の離脱と終了

**離脱（Welcome へ戻る）と終了（プロセスを終える）は別の答えである**。どちらも Home の
[exit prompt](#feedback-と終了)（`Ctrl-Q`、live pane 上の `Ctrl-C`）から選ぶが、選択肢・キー・
効果が分かれているため、片方のキーを打ち間違えてもう片方に落ちることはない。

| 答え | キー | 効果 |
|---|---|---|
| `welcome` | `w` | この workspace を離れて Welcome へ戻る。プロセスは続く |
| `quit` | `q` / `y` | この TUI client を終了する |
| `stay` | `n` / `Esc` | この workspace に留まる |

離脱と終了はどちらも **この workspace のために確立した資源をすべて落とす**。terminal lane・poll lane・
pane launch client・restore client の接続、Home の 3 つの[背景観測 lane](#home-frame-loop-と背景観測-lane)、
metrics lane はいずれも workspace の frame loop が所有しており、loop を抜けることが teardown そのものである。
したがって**次の workspace を開く時点で、前の workspace の port・pump・worker は 1 つも残っていない**。
唯一の例外は restore observation の client で、これは「hung な restore を終了が待たない」ために
切り離した worker が持つ（[背景 observation lane](#背景-observation-lane)）。

daemon-owned の terminal と operation は離脱でも停止しない。**離脱の意味は detach と同じ**であり、接続を
閉じることで daemon が subscription を解放する（[connection epoch と subscription 無効化](#connection-epoch-と-subscription-無効化)）。
専用の detach request は送らない。

**プロセス内で同時に接続する daemon は 1 つだけである**。daemon が serve する workspace は 1 つに確定して
いるため（[workspace の選択と daemon](#workspace-の選択と-daemon)）、離脱で旧 workspace の接続を完全に閉じてから
次の workspace の daemon へ接続し直す。複数 daemon への接続を同時に持つことはない。戻った Welcome から
別 workspace を選んだときの fence 拒否も、起動直後と同じく**その画面に留まって notice に出す**。無言で
前の workspace へ戻ることはしない。

戻り先の Welcome は**開いた時点の Recent 順序を保つ**。workspace を開いた時点で `record_opened` 済みなので
離れた workspace は先頭にあり、entry 画面が daemon も store も読み直さない原則（[workspace の選択と
daemon](#workspace-の選択と-daemon)）をそのまま守る。ただし `usagi <path>` / `usagi open <path>` のように
workspace を直接開いた入口には背後に Welcome が無いため、離脱時に合成ルートが Recent を読み直して
entry 画面へ入る。

## settings scope と workspace entry

TUI settings の保存先と解決順序は次のとおりである。この節が v2 settings scope の正本である。

| 設定 | 保存先 | 読み取り・反映 |
|---|---|---|
| Global | build channel ごとの user data directory にある `settings.json` | Theme・Modal mode・Environment はすべての workspace に適用する。Agent・Issue・Memory は新規 workspace の初期値として使う。ファイルが無ければ core `Settings` の既定値、欠損 field と未知 enum token も field ごとの既定値へ縮退する |
| Workspace | 対象 repository の `.usagi/settings.json`（development mode は `.usagi/dev/settings.json`、local mode は `.usagi/local/settings.json`） | Agent・Issue・Memory だけを保持する。workspace 登録時に Global の初期値を一度コピーし、以後の Global 変更は反映しない。欠損 field と未知 token は安全な互換動作として Global を継承する |

Config の保存は対象 scope の cross-process lock 内で最新 settings を読み直し、画面が所有する field だけを draft から
merge して atomic write する。Global Config は Theme・Modal mode・Agent・Issue・Memory を所有し、Environment 行の
editor は global `env` だけを同じ scope lock 下で保存する。通常の Config 保存は `env` と `local_llm` を保持する。
Workspace Config は Agent・Issue・Memory と workspace `env` を所有する。workspace の Environment editor は
workspace scope だけを読み書きし、global `env` を表示・変更しない。
同じ owned field を複数の Config が並行して変更した場合は、lock を取得して最後に保存を完了した draft を採用する。

Agent は `default_model`、Issue と Memory はそれぞれ `issue_enabled` / `memory_enabled` として保存する。
`default_model` は選択可能な agent CLI の closed vocabulary（`claude` / `codex` / `sakana.ai`）であり、Config 画面の
Agent 行と Closeup の [`agent -m`](#closeup-の-agent-cli-選択) が同じ語彙を共有する。`sakana.ai` は Codex 互換 CLI で、
実行するのは `codex-fugu`（daemon profile は `sakana-ai`）である。
Issue と Memory の Global 初期値はどちらも `true` である。Workspace ファイルに残る旧 Theme / Modal mode field は読み飛ばし、
全体設定を上書きしない。
Workspace の Agent・Issue・Memory は個別値を持ち、MCP server は起動時に解決した実効値を tool 公開・実行へ適用する。
無効時の tool 範囲と server lifetime の契約は [MCP サーバ](07-mcp.md#tool-面)を正本とする。

Workspace 設定の保存は project store の cross-process lock を取得し、version envelope を付けた local
settings 形式へ temp file、fsync、rename の順で atomic write する。形式 migration は発生しない。壊れた JSON や
読み取り不能なファイルは Config では load error とし、既定 draft と error notice を表示して保存前の値を暗黙に
上書きしない。Home entry は workspace local の読み取り失敗時に Global、Global も読めない場合は core の既定値へ
縮退し、設定ファイルの破損だけで workspace を開けなくしない。

direct workspace、Welcome の Open / Recent、New の作成成功は、snapshot の workspace path を identity として
settings port を毎回束縛し直す。次に Global とその workspace の Local を解決し、effective な Modal mode を
Overview / Closeup を生成する Home runtime へ渡す。この束縛は workspace entry ごとの lifecycle であり、直前に
開いた workspace の port や modal state を次の workspace へ持ち越さない。Config へ入るたびにも現在の束縛から
両 scope を読み直すため、保存後の再 entry とプロセス再起動で同じ値になる。

## workspace の選択と daemon

session 一覧・scope・PR inventory は daemon が権威であり、**daemon が serve する workspace は起動時に確定した
1 つだけ**である（[5. daemon](05-daemon.md#daemon-process-lifecycle)）。一方 TUI が開く workspace は、起動した
directory ではなく利用者の選択（`usagi open <path>` / `usagi <path>`、Welcome の Recent、Open 一覧、New の作成
成功）で決まる。この節はその 2 つを一致させる契約の正本であり、wire の申告と admit 条件は
[4. daemon IPC#workspace fence](04-ipc.md#workspace-fence) が正本である。

**TUI は開いた workspace を申告する**。workspace 画面のために daemon へ出す最初の request の前に、選択した root を
canonical 化して `selected` として申告するため、daemon は「serve していない workspace の一覧を返す」ことができない。
したがって *ある workspace の title の下に別 workspace の session 一覧* は表示されない。

| 状況 | 挙動 |
|---|---|
| daemon が動いていない | 選択した workspace で daemon を起動する（起動する lifecycle child の cwd が選択した root になる）。起動した directory に束縛された daemon は作らない |
| daemon が選択した workspace を serve している | そのまま開く。TUI の起動 directory は問わない（workspace 内・subdirectory・session worktree・workspace 外のいずれでもよい） |
| daemon が別の workspace を serve している | 開かずに拒否し、serve している workspace root と復帰手順（`usagi daemon stop` して目的の workspace で起動する）を提示する。registry への登録も Recent の更新も行わない |
| 選択した path が UTF-8 でない | 開かずに拒否する。daemon の権威記録（`sessions.json`）と workspace registry はどちらも JSON なので、その root は書き留められず**どの daemon も所有できない**。daemon を起動もしない |

拒否の提示先は入口ごとに異なるが、内容は同じ 1 つの message である。

| 入口 | 提示 |
|---|---|
| Welcome の Recent、Open 一覧 | その画面に留まり notice に出す。折り返して全文を表示するので理由と手順が切れない。続けて serve されている workspace を選べる |
| New の作成成功後の open | draft を保ったまま同画面の notice に出す |
| `usagi open <path>` / `usagi <path>` | TUI を開かず stderr へ 1 行で出す |

entry 画面（Welcome・Open・New・Config）は **daemon を必要としない**。表示に使うのは registry と Recent という
local store だけであり、workspace 切り替え画面はどの directory からでも開ける必要がある。ここで daemon の readiness を
確かめると、起動 directory に束縛された daemon を作ってしまい、その後のどの workspace の open も拒否されることに
なるため、daemon 接続は workspace を開く時点まで遅らせる。表示専用の daemon metrics も同じ理由で daemon を起動せず、
daemon が居なければ metrics 無しで動作する。

## Production screen graph harness

TUI の production wiring は、direct Workspace、Welcome の Recent、Open の選択、New の作成成功を
`run_screen_graph_with_backend` の同じ deterministic harness で検証する。harness は terminal、workspace loader、
settings、controller backend factory、Agent/terminal port を注入し、実端末や daemon socket を開かずに、各入口が
同じ settings と production port set を受け取ること、全 Effect route、成功・失敗 completion、`Ctrl-O` を含む
global chord、terminal reconnect を固定する。unit test module の harness 組み立て自体は production code ではないため
`composition` 例外とし、そこで駆動される controller・presentation・runtime mapping は coverage 対象に保つ。

実端末の raw mode / draw / read、daemon socket、filesystem、platform process の薄い adapter だけを `real_io`、
production port の束縛だけを `composition`、fake port の generic 単相化だけを `generic_monomorphization` として
機械可読コメント付きで除外する。reducer、Effect routing、entry selection、input classifier、completion / error
projection は除外しない。例外の理由・期限・証拠 test は
[coverage exclusion policy](06-conventions.md#coverageoff-例外)に従い、`scripts/coverage-off-lint.rb` で検査する。

各 Effect は実 action を 1 回開始するか、安全な明示 error completion を 1 件返す。session create は成功時も
要求 token と作成された `SessionId` を持つ `OperationResult` を返す。失敗だけを返して成功を snapshot 更新に
暗黙化しない。terminal の `open` は同じ target の live terminal を再利用し、`new` は target の worktree を
cwd に native terminal を開くため、controller は両者を別 effect として host まで保持する。notes と environment の保存は target の集合全体を永続化し、
decision / PR / browser / notification は daemon または platform adapter の結果を controller へ還流する。
従来 silent no-op だった操作も成功扱いせず、画面に安全な結果を返す。永続データの migration は発生しない。

対話的な `usagi` / `usagi hop` の Welcome 起動時は、110ms 間隔で 14 フレームのスプラッシュを再生する。
ピンクの usagi を先に表示し、`USAGI` を暗い緑から Success の太字へフェードインしてから Welcome を描く。
非対話環境と `usagi config` はスプラッシュを再生しない。

スプラッシュは**スキップできる**。この 2 つを両方満たす。

- **打鍵で中断する**。フレーム間の待機は「その時間を上限に入力を 1 つ待つ」ため、キーが届いた時点で
  残りのフレームを捨てて Welcome を描く。**中断に使ったキーはスキップとして消費する**（「何かキーを
  押すと飛ばせる」の標準的な契約）。以前は入力を読まずスプラッシュ中の打鍵を Welcome の最初の入力へ
  渡していたが、これは端末由来の紛れ込んだバイトも同じように次の画面へ流し込むため、消費する側に
  倒した。起こし待ちの tick と端末リサイズは打鍵ではないので、アニメーションの速度を保つ。
- **1 プロセスで 1 回だけ再生する**。[workspace を離れて戻った Welcome](#workspace-の離脱と終了) は起動では
  ないため、2 回目以降は 1 フレームも描かない。中断しても Welcome の初期状態は変わらない。

実端末は raw mode、alternate screen、cursor、mouse、自動折返しを合成ルートで管理する。TUI は端末非依存の
event stream を reducer に渡し、frame diff だけを返す。TUI の実行中は自動折返しを無効化し、右下セルへの描画が
スクロールを起こさないようにする。resize は前 frame を無効化して全体を再描画し、終了時は端末属性、折返し設定、
alternate screen を復元する。

## Home と target

Home の navigation target は managed `Session(SessionId)` である。表示名と配列 index は identity に
使わない。selected は cursor、active は command と Closeup の managed session であり、cursor の移動だけでは
active を変更しない。session が 0 件なら selected は `+ new session`、active は `None` となる。

Closeup の `agent` / `terminal` action は active managed session の `SessionId` から scope を導出する。
managed session が無い Home は Closeup を開かず、これらの effect を発行しない。`Target::Root` と
`session_id: None` は daemon の workspace-root scope 語彙として残るが、通常 Home の Closeup entry からは
生成しない。

daemon snapshot または lifecycle refresh で selected / active session が消えたか使用不能になった場合、
表示順上の surviving session へ決定的に着地する。surviving session が無ければ selected は
`+ new session`、active は `None` となり、削除済み session を target にした古い local state を実行に使わない。

Home の mode は Switch と Closeup である。**右ペインは、その pane が入力を所有していない間つねに dim で
表示する**（tab strip、content、footer を含む）。active な明度へ戻すのは、Closeup で選択中の tab が live
terminal であり、かつその前面に何も無い frame だけである。したがって次はすべて dim になり、その間の右ペインの
scroll、tab close / reorder、text selection、copy、link open は入力を受け取らない。

| dim になる frame | 入力の所有者 |
|---|---|
| Switch | 左 sidebar |
| Closeup で pending / interrupted tab を選択中（live terminal viewport が無い） | Closeup の management input |
| overlay・Closeup action modal が前面にある | その overlay |
| [指示モード](#指示モードdirector-mode)の drawer が開いている | drawer の root conversation |

Overview、Closeup action、PR、preview、text、notes、
environment、pending user decision、session 作成失敗 dialog は Home の背景を残す overlay として開き、最前面の overlay が入力を受け取る。diff は
Closeup pane の tab として開く。

### Switch の右ペインは cursor の preview

Switch は左 sidebar が navigation を持つため、右ペインは cursor（hover）が指す session の preview である。
見出しの session 名、tab strip、agent phase 行、live terminal の viewport はいずれも cursor 行に追従し、
footer は `[Switch] preview pane` と表示して、まだ command の対象ではないことを示す。Closeup は active
managed session を描き、footer は `[Closeup] active pane` になる。

| mode / cursor 行 | 右ペインが描く対象 |
|---|---|
| Switch・session 行 | その session（cursor が指す hover 対象） |
| Switch・`+ new session` 行 | active managed session（session を指していないため target へ退避する） |
| Switch・session 行・Director drawer が開いている | その session（drawer の背景でも cursor が指す hover 対象を維持する） |
| Closeup | active managed session |

preview は表示だけを移し、command target（active）と live PTY 入力の宛先は動かさない。cursor の移動が
active を変えないのは [Home と target](#home-と-target) のとおりで、Switch は PTY へキーを流さないため、
入力は常に active target の focus 済み tab へ向かう。一度も開かれていない session を hover した場合は、
未起動 target と同じ空の pane を描く。client が daemon へ attach する foreground terminal は preview に
追従するため、同時に attach する live terminal は従来どおり 1 つである。Director drawer が開いている間だけは、
背景の右ペインが Switch の cursor に追従したまま、foreground terminal の attach と入力を drawer で選択中の
root conversation が所有する。右ペインは detach 前に保持した managed terminal の viewport を dim のまま描き、
root conversation へ foreground attach を移しても背景の Agent content を空にしない。

Pending user decision は workspace ID で fence した daemon snapshot からだけ投影する。overlay は pending
一覧を表示し、選択すると title、prompt、option label/description、期限、freeform が許可された場合だけその
editor を表示する。Esc は editor から一覧へ戻り、一覧では overlay を閉じるだけで durable decision を変更しない。
submit は stable option ID または空でない許可済み freeform を送る。row は daemon の resolve confirmation まで
残り、resolve error・disconnect・resync 後も snapshot で再試行可能な pending state に収束する。modal が開いて
いる間は Home、Closeup、terminal の背景入力を dispatch しない。

decision の title、prompt、option label/description、freeform は modal 幅で折り返す。表示域を超える editor の
内容は `PageUp` / `PageDown` で読み進め、`↑` / `↓` による option 選択へ戻ると選択中の行へ表示を戻す。

新しい pending decision を resync で観測すると、Home header の右上に `🔔 N notice` を表示し、その直下の
banner に session identity（root は `workspace root`）と decision の title（summary）を表示する。ベルをクリックすると existing decision modal を
開き、未読表示を既読にする。modal が前面の場合はベル・banner を含む背景入力を受け取らない。未読は TUI-local の
stable decision ID 集合であり、同じ snapshot の replay、reconnect、resync は再び未読にしない。decision が
resolve/cancel/expire で pending snapshot から消えると未読も消える。

TUI は tick の resync で初回 snapshot 後に新規 ID を観測したときだけ desktop notification port を呼ぶ。通知本文は
session identity と title のみで、prompt・option・freeform answer は OS notification に送らない。port の配信失敗は safe に
無視し、TUI の banner / modal を継続する。合成ルートは macOS では `osascript`、Linux では `notify-send` を固定の
実行ファイルと引数ベクトルで spawn する。その他の OS、実行ファイル不在、headless notification service の失敗は
非対応として no-op である。notification、browser、external terminal の helper process は合成ルートの共通 reaper が
上限付きで追跡し、TUI thread では待たずに `try_wait` で非同期に回収する。追跡枠が満杯なら新しい helper は spawn
せず、notification は no-op、browser と external terminal は既存の safe feedback として扱う。長寿命の external
terminal launcher が残っていても、短命 helper の回収は独立して進む。終了時は新規受付を止め、終了済み child を
回収するが、起動中の外部 application は kill しない。

Home 背景の dim は各 ANSI span の reset（`0` / 省略形）と通常輝度指定（`22`）の後にも維持し、行末で必ず
reset する。入力を所有しない右ペインでは terminal 内 UI の focus 表現である bold、blink、reverse、背景色と
物理カーソル位置を抑止する。SGR の faint は背景色へ効かない terminal があるため、focus 行だけが明るく残る状態を
許さない。overlay は dim 済みの背景へ後から合成するため、modal 自身の style と可読性を優先する。

左 sidebar の marker は Home target 表示の正本である。Switch では selected cursor と current
target を別々に stable identity から照合し、同じ行なら cursor を優先する。Switch の cursor ではない
session / `+ new session` 行は v1 と同じ dim の非アクティブ色で描き、selected session の Accent は
保つ。Closeup では session を Accent で描き、current session だけを太字にする。`+ new session` は
色付けされるときは常に Success（緑）で、accent（青）へは決して落ちない。cursor が乗る Switch の選択時は
Success の太字、Closeup は Success の非太字で描き、太字は Switch の選択時だけに限る。Switch で cursor が
乗っていない `+ new session` は上記の非アクティブ dim に従う（この dim だけが Success 色を上書きする）。この Success 色は
full sidebar 行・rail の `+`・右ペイン preview 見出しで共有する単一の役割決定であり、生の ANSI 色ではなく
意味的 palette 役割で描くため、theme を retune しても追従し accent（青）へは落ちない。Closeup では cursor を
描かず、current marker だけを残す。session cursor はうさぎ `󰤇` と太字の名前、
`+ new session` は Switch で選択されていても chevron を描かない。cursor ではない current target は緑の `▎`
で示す。`+ new session` と pending
skeleton は current target にならない。名前・補足・marker は ANSI を閉じた表示幅で clip/pad するため、
CJK、Nerd Font glyph 未対応、極小幅でも後続行の style や列幅を壊さない。

Switch で cursor ではない session の補足行は、相対時刻・PR・Git summary の意味色を保ったまま dim にする。各
ANSI span の reset 後にも dim を再適用するため、current marker や Git の色 span が続いても相対時刻だけが明るく
戻らない。cursor が指す session の補足行は、相対時刻・PR を dim のまま描く一方、Git summary の意味色を通常輝度で
描き、選択対象の Git 状態を非アクティブ行と区別する。

Home controller の management input では、Switch の `Ctrl-A` は新規 session 作成フォームを開く。session 行を
選択中の `x` は `session remove`、`Shift`+`x`（`X`）は `session remove -f` を実行する。`+ new session`
行では削除しない。`Ctrl-Q` は exit prompt を開く（離脱と終了の区別は
[workspace の離脱と終了](#workspace-の離脱と終了)）。Switch の `Ctrl-C` は何もしない。Closeup の live pane でも、leader が
待機していない `Ctrl-C` / `Ctrl-Q` / `Ctrl-D` は global shortcut として management transition に渡す。Closeup の `Ctrl-O o` は
Switch へ戻り、Switch 中の `Ctrl-O` は単体では mode を変えない。Closeup action modal が前面にある間の `Esc` /
`Ctrl-C` は、`Ctrl-O o` と同じく modal を閉じて Switch へ戻る（live pane の有無に依らない）。overlay を開いて
いない Closeup の live pane 上の `Ctrl-C` が exit prompt を開く契約はそのままである。前面 overlay は共通入力境界で
`Ctrl-C` / `Ctrl-Q` を route より先に所有し、通常は overlay に留まる。例外は `Ctrl-C` で Switch へ戻る Closeup action
modal と、`Ctrl-C` を acknowledge として閉じる session 作成エラーだけであり、いずれも TUI の終了には伝播しない。

左 sidebar は、実 session・`+ new session` の左クリックで cursor だけを移し、active session や mode を
変更しない。実 session は、同じ stable `SessionId` を 400ms 以内（境界を含む）にもう一度左クリックした場合だけ、
Enter と同じくその session を active target にして Closeup を開く。座標や表示順は同一性の判定に使わないため、
scroll や daemon snapshot によって同じセルの session が入れ替わっても Closeup を誤って開かない。
`+ new session`・mascot・footer はダブルクリックの対象外であり、それらへの click は直前の session click と
結合しない。modal と inline 作成中は背景の sidebar click を受け取らず、その前後の click も結合しない。daemon
snapshot で session 一覧を置き換えた場合も、置換前後の click は同じ `SessionId` が残っていても結合しない。

Closeup の入力所有者は tab の有無で決まる。tab が無い Closeup は management input が所有し、action modal を
前面に出す。tab が 1 つ以上ある Closeup は `LiveInputClassifier` がすべての入力を先に分類する。pending な `Ctrl-O`
prefix（leader）が次の入力を所有し、leader が無い場合だけ `Ctrl-C` / `Ctrl-Q` / `Ctrl-D` を global shortcut として解決する。
それ以外の非 prefix 入力は、修飾キーを含めて live terminal への passthrough として扱う。leader の follow-up は下表のアクションに
解決し、それ以外は消費する。tab 切替（`Ctrl-O` / `Ctrl-A` / `Ctrl-N` / `Ctrl-P`）は reducer が所有するが、scroll・tab close・copy は
reducer に持ち込まず shell と `TerminalSession` が所有する（scroll offset・選択・feedback は shell 側の状態）。

controller reducer path も同じ投影を使う。**tab を 1 枚も持たない** target の Closeup への遷移だけが action overlay を
自動で開き、pane が到着すると通常の tab surface へ戻る。live PTY を持たない tab（interrupted Agent history）だけを
持つ target も tab surface へ着地するので、`Ctrl-O` の pane control はそのまま届く。最後の live tab が exit しても
tab が残っていれば action overlay へは戻らない。runtime は `PaneTabAvailability` を `LivePaneAvailability` より先に
sample するため、live pane を失った時点の判定は現在の tab 有無を見る。adapter は prefix の next / previous 結果を
`CtrlN` / `CtrlP` として reducer に渡し、reducer は pane 所有者へ tab selection effect を要求するだけで、tab
identity は保持しない。tab 巡回は live PTY の有無ではなく tab の有無で有効になる。

| prefix | アクション | 効果 |
|---|---|---|
| `Ctrl-O` `Ctrl-O` | Switch | Closeup から Switch へ戻る |
| `Ctrl-O` `Ctrl-A` | OpenCloseupModal | Switch では選択 target の Closeup action を開く。Closeup では tab があっても action modal を前面に出す |
| `Ctrl-O` `Ctrl-N` | NextTab | 次の tab を選ぶ（[指示モード](#指示モードdirector-mode)が開いている間は New） |
| `Ctrl-O` `Ctrl-P` | PreviousTab | 前の tab を選ぶ |
| `Ctrl-O` `Ctrl-G` | Director | [指示モード（Director mode）](#指示モードdirector-mode) を toggle する |
| `Ctrl-O` `n` | DirectorNew | 指示モードを開き、明示的な New CLI picker を表示する（[指示モード](#指示モードdirector-mode)が開いている間は NextTab） |
| `Ctrl-O` `]` | MoveTabNext | 選択 tab を次の表示 slot へ移動し、Agent 順序を commit する |
| `Ctrl-O` `[` | MoveTabPrevious | 選択 tab を前の表示 slot へ移動し、Agent 順序を commit する |
| macOS: Command+C / Linux: Ctrl+Shift+C / Windows: Ctrl+C | Copy selected output | 保持中の terminal 出力選択を OS clipboard へ再コピーする |
| `Ctrl-O` `x` / `Ctrl-O` `Ctrl-X` | CloseTab | 選択中の tab を閉じる（live なら subscription を detach、pending なら起動待ちを取消、[interrupted](#interrupted-agent-の-tab-投影と明示-resume) なら lineage を dismiss） |
| `Ctrl-O` `r` | ResumeTab | 選択中の [interrupted tab](#interrupted-agent-の-tab-投影と明示-resume) を明示 resume する（他の tab は変更しない） |
| `Ctrl-O` `u` / `↑` | ScrollUp | 右ペインの scrollback を 1 行古い方向へ |
| `Ctrl-O` `d` / `↓` | ScrollDown | 右ペインの scrollback を 1 行 live bottom 方向へ |
| `Ctrl-O` `b` / `End` | ScrollBottom | 右ペインを live bottom へ 1 手で戻し、新しい出力への追従を再開する |

follow-up の plain `n` / `Ctrl-G` / `x` / `Ctrl-X` / `[` / `]` / `u` / `d` / `b` / `↑` / `↓` / `End` は leader が生きている間だけ予約し、leader 無しの単体キーは PTY へ送る。
classifier は plain `n` を New、`Ctrl-N` を NextTab として修飾状態で区別する。この 2 つの意味だけは
**指示モードの drawer が開いている間に入れ替わる**（`Ctrl-O Ctrl-N` が New、`Ctrl-O n` が conversation の
NextTab）。入れ替えは frame loop が key を 1 度だけ retarget するので、PTY 転送・pane control・reducer は
同じ 1 つの key を見る。classifier 自体は drawer の状態を持たない。
`Ctrl-O` 後の plain `g` は drawer action ではなく PTY へ 1 回だけ送る。leader は 1 秒で失効し、その他の未知の
follow-up、key release、raw byte を含む次の入力を 1 件だけ握って捨て、その時点で必ず reset する。
auto-repeat は press と同じ follow-up として 1 件だけ解決する。ちょうど 1 秒の timeout 境界では leader は失効済みであり、単一 raw
control byte と semantic control event は同じ global shortcut に解決する。

Windows の `Ctrl+C` は terminal 出力を選択中なら copy とし、選択が無い場合は PTY へ SIGINT として送る。

## 指示モード（Director mode）

root scope（`session_id: None`）の Agent へ指示を出し、session を作らせる面を**指示モード**（英語 / identifier は
`director`）と呼ぶ。この節が指示モードの名称と仕様の正本である。managed session の実作業を見る面
（[Closeup pane](#closeup-pane)）とは役割が異なり、指示モードは Home header の下から右端へ重なる drawer として現れる。

Home header の右端には Nerd Font の robot glyph を使う `[ 󰚩 director ]` button を表示し、drawer title も
`󰚩 director` とする。glyph は既存の CPU / memory / mode icon と同じく直接描画し、対応しない font や狭幅でも
Unicode display width による clip と hit-test を維持する。workspace breadcrumb、mode toggle、
pending decision の notice badge、button は 1 つの header layout が表示幅と click range を同時に計算する。
そのため CJK workspace 名や notice の有無、狭幅による breadcrumb の clip があっても、描画された button / badge
と hit-test は同じ terminal cell を指す。

button の強調は mode toggle と同じ「前面にある面がアクセント」の対比に従う。drawer が閉じているときは
選択されていない mode chip と同じ dim で描き、Switch / Closeup のどちらでも accent を持たない。drawer を開いた
frame だけ accent + reverse になり、前面の面が一意に読める。狭幅で mode toggle を落として button だけを
clip する場合も、この対比は変わらない。

button または `Ctrl-O Ctrl-G` は、Switch、managed-session Closeup、live pane のいずれからも同じ
指示モードの open/closed state を toggle する。drawer の通常幅は端末幅の 60% とし、
56 columns 以上 96 columns 以下へ clamp する。56 columns の drawer と 24 columns の背景を
同時に保てない幅では全幅へ縮退する。背景 Home は ANSI span ごと dim にし、header は表示したままにする。
drawer 内の terminal viewport は drawer の border、conversation selector、spacer、footer を除いて計算し、
managed-session Closeup の right pane viewport とは別の pure geometry とする。

drawer は root scope（`session_id: None`）の live / pending / interrupted Agent conversation だけを
conversation selector に表示する。generic Terminal、Diff、Terminal pending/action は restore projection と pane
admission の両方で拒否する。live Agent の continuation が intent context 未作成、未 observe、CAS 後の投影遅延で
まだ得られない場合も terminal fence を identity として selector に残し、provider metadata を含まない `Agent` を
fallback label にする。terminal view がある frame は conversation inventory の有無にかかわらず PTY 出力を描き、
terminal view も conversation も無い場合だけ empty state を描く。drawer が閉じている間の `Ctrl-O n`、開いている
間の `Ctrl-O Ctrl-N`、または `[ New ]` の mouse-down hit で drawer を開いて
合成ルートから注入された install 済み CLI だけを `claude`、`codex`、`sakana.ai` の順で picker に表示する。
設定済み default が候補ならそこを、なければ先頭候補を highlight するが、自動確定はしない。`↑↓` は循環選択し、
`Enter` は選択した CLI の explicit profile を確定する。`Esc` は conversation order / selection と drawer open
状態を変えず picker だけを閉じる。候補が 0 件なら picker を開かず、installation と Config の確認を促す
safe empty state を表示し、daemon request を発行しない。

picker の viewport は selection に追従し、候補が picker の行数を超える端末でも highlight 中の候補を必ず描く。
窓の外に残る候補は `↑ N more` / `↓ N more` へ畳むが、この indicator は候補と同じ行数を分け合うため、
両立できない高さでは候補行を優先して indicator を落とす。候補を 1 行も置けない高さ（drawer chrome が
端末の 6 行を占めるため 6 行以下、80x6 相当）では footer を `Terminal too short to choose` に替え、
`Enter` は launch を発行しない。表示されていない CLI が起動することはない。

picker の確定は fresh `OperationId`、`session_id: None`、選択した explicit profile を持つ既存 daemon Agent
launch path を 1 回だけ呼ぶ。TUI は argv / cwd / provider model path / secret を組み立てない。request 前に root
pending slot を 1 枚作り、同じ operation・semantic digest・root scope を持つ successful final の exact
`TerminalRef` と continuation だけを live conversation へ昇格する。operation が完了するまで New を fence するため、
double Enter、duplicate completion、reconnect replay は 1 request / 1 tab へ収束する。
root New の pending / completion は terminal の `workspace_id` と `session_id: None` を
`Target::Root` に照合してから root registry entry だけへ admit する。scope が一致しない completion は拒否し、
現在 active / selected な managed-session entry へ fallback しない。New を繰り返しても増えるのは drawer の
conversation だけで、managed Closeup の tab count・identity・selection は変わらない。

成功時は root `AgentTabIntent` の order への追加と新 conversation の selection を 1 回の CAS mutation で commit
してから pending slot を live にする。write / CAS / future-schema failure、profile rejection、daemon 不通、
wrong workspace / session / operation / semantic final は pending を失敗表示へ遷移させ、既存 conversation・selection・
terminal bytes を変更しない。daemon runtime が既に成功していて intent の commit だけが失敗した場合も、TUI は
local success を捏造せず、既存 intent contract の safe error を表示する。

coherent inventory は drawer が閉じていても root target の `AgentTabIntent` と reconcile し、保存済み order /
selection を準備する。保存済み exact ref が trusted live なら同じ `TerminalRef`、同 lineage が
resumable なら同じ slot の interrupted tab を投影する。inventory-only conversation は deterministic order で末尾へ
加え、消失した selection は同 slot より後の surviving tab、なければ先頭、conversation が無ければ empty state へ
縮退する。drawer 自体は observation から自動 open しない。

drawer open 時は root の selected live Agent だけを foreground attach し、drawer 専用 viewport geometry を使う。
選択中 Agent へ既存の ordered input / ACK、terminal-local な scroll / selection / feedback、copy / link を接続する。
他の root tab と managed pane は detached background である。drawer close 時は root subscription を detach し、
開く前の managed-session selected live tab を元の right pane geometry で attach する。detach 中の terminal は別 pane の
geometry へ resize せず、**attach 自体がその pane の viewport を宣言する**。daemon は detach と一緒にその window の
[共有 viewport](05-daemon.md#共有-viewport複数-client-の-geometry) の要求を捨てるため、再 attach では毎回宣言し直す
必要がある（黙って attach すると、他 window の小さい viewport がその window の終了後も残ってしまう）。宣言は
attach request に載るので追加の往復は無く、`Resize` は pane の実サイズが変わったときだけ送る。
どちらの操作も PTY/process を kill / spawn しない。terminal coordinator は bounded cache に保持するため、同じ connection epoch
上の再 attach は `input_seq`、未収束 input fence、その後ろの queue、復号済み screen を引き継ぐ。cache から
eviction された terminal も attach 応答の `next_input_seq` を採用し、daemon ledger より前へ巻き戻さない。
connection epoch が変わった場合だけ sequence は daemon とともに 0 へ戻る。

interrupted tab は read-only で、open / reconnect / restore から provider resume を発行しない。選択中 interrupted
tab の `Ctrl-O r` だけが既存の exact resume contract を実行し、operation / source / relation / lineage / root scope /
new exact `TerminalRef` がすべて一致した成功だけを同 slot の live Agent tab へ置換する。drawer を閉じている間に
応答した置換は root background entry だけを更新し、managed foreground を奪わない。

drawer open 中は drawer が sidebar、managed pane、Home header の別 action、通常の global action の入力を所有し、
それらへ key / click / pointer を伝播しない。root Agent tab の terminal input と `Ctrl-O` tab controls、および
New picker の `↑↓` / `Enter` / `Esc`、`Ctrl-O Ctrl-N` の New、`Ctrl-O Ctrl-G` の close だけを受理する。picker が閉じている
間の通常文字・`Enter`・`Esc` は root Agent terminal へ送る。`[ New ]` の mouse-down は
drawer が先に消費して picker を開き、同じ pointer gesture を背景 Closeup の click / focus / attach 選択へ
fallthrough させない。picker Choosing 中の `[ New ]` 再クリックは inert とし、mouse-up も背景へ渡さないため、
launch は明示的な `Enter` だけが発行する。開閉は Home mode、selected cursor、active managed session、
managed pane の selected tab、terminal scroll / text selection を変更しない。既存 modal が前面にある間は drawer
shortcut と header button を受理しない。drawer open 中の root foreground availability は背景 Closeup modal を
開かず、modal と drawer は同時に visible にならない。

**`Esc` は selected root Agent が所有する**。agent CLI は `Esc` を自身の中断・取消として読むため、live
conversation が attach している間の `Esc` は drawer を閉じず、その PTY へ `0x1b` を 1 回だけ送る。drawer を
閉じるのは `Ctrl-O Ctrl-G` と header button である。`Esc` が drawer の close になるのは、`Esc` を受け取れる
live conversation が無い frame（conversation が空、または pending / interrupted tab だけを選択中）に限る。
picker が開いている間の `Esc` は picker だけを閉じ、PTY へは届かない。

New picker の `Choosing` / `Empty` と launch pending (`Launching`) は排他的な foreground input owner である。この owner は
上記の picker 予約操作以外の keyboard / `Char` / paste / terminal copy / pointer と、tab の選択・移動・close・resume、
terminal scroll を inert に消費する。したがって背後の root Agent PTY bytes、pane/tab state、scroll、text selection、
attach/detach は変化しない。terminal resize と backend/timer tick だけは owner を越えて通常の frame 処理へ進む。
`Esc` で picker を閉じた次の input から drawer conversation の規則へ戻り、通常文字・paste・`Enter` は selected root
Agent PTY へ送る。

入力 context の優先順位と遷移は次のとおりである。

| 現在の context | 入力 | 次の context / effect |
|---|---|---|
| modal | drawer chord / button | modal を維持し、drawer は開かない |
| drawer conversation | `Ctrl-O Ctrl-G` / header button | drawer を閉じ、元の route / managed pane selection / focus を復元する |
| drawer conversation（live Agent あり） | `Esc` | selected root Agent PTY へ `0x1b`。drawer は開いたまま |
| drawer conversation（live Agent なし） | `Esc` | drawer を閉じ、元の route / managed pane selection / focus を復元する |
| drawer conversation | `Ctrl-O Ctrl-N` / `[ New ]` click | drawer picker。背景への pointer / key effect は発行しない |
| drawer conversation | `Ctrl-O n` / `Ctrl-O Ctrl-P` | conversation の次 / 前を選ぶ |
| drawer conversation | 通常文字 / `Enter` | selected root Agent PTY。New picker は開かない |
| drawer picker | `↑` / `↓` | picker 内の CLI 選択だけを循環する |
| drawer picker | `Esc` | picker だけを閉じ、drawer conversation に戻る |
| drawer picker | `Enter` | root scope launch を 1 件発行し、drawer を最前面に保つ |

## Home frame loop と背景観測 lane

**描画スレッドは daemon を同期で叩かない。** Home の 1 frame は `非ブロッキング drain → 純粋な projection → draw →
入力` だけで構成し、daemon への request はすべて背景 lane が発行する。この不変条件は Home の 3 lane（decision /
session / metrics）と、live terminal の
[背景 observation lane](#背景-observation-lane)（foreground poll pump / background inventory pump）に共通である。

Home の 3 lane はそれぞれ**専用の常駐 worker thread と専用の永続接続**を持ち、cadence は 250ms〜1s の範囲に clamp する。
worker は workspace を開いたときに 1 本ずつ起動して閉じるまで生存するため、frame が thread を作ることはない。

| lane | 観測対象 | primitive | cadence | 起動する契機 | cold-start |
|---|---|---|---|---|---|
| decision | 保留中の user decision | `UserDecision::List` | 500ms。失敗中は 500ms から 8s 上限の指数 backoff | 最初の `RefreshDecisions` | しない |
| session | 他 client（MCP server / CLI）が変えた session lifecycle | `Session::List` | 1s。失敗中は 1s から 8s 上限の指数 backoff | frame loop 開始時の wake | する（高々 3 回） |
| metrics | mascot の daemon metrics | `Metrics::Snapshot` | 1s。失敗中は 1s から 8s 上限の指数 backoff | 最初の描画 | しない |

- **lane は駆動されるまで dormant である**。worker thread は composition と同時に起動するが、上表の契機で起こされるまで
  request も接続も発行しない。したがって composition を組み立てただけで daemon IO は起きない。
- **request rate は frame rate に比例しない**。idle な Home が生む daemon request は上表の cadence だけで決まり、
  接続確立は lane あたり workspace の生存中に原則 1 回（transport 断のあとの再接続のみ追加）である。
- **同種の未処理要求は最新 1 件に畳む**（coalesce）。lane の in-flight request は高々 1 件で、cadence 1 周期に何度
  refresh を要求しても発行される request は 1 件である。畳んだぶん、他 client が作った session の反映は最大で
  cadence 1 周期ぶん遅れる。これは受け入れる遅延であり、利用者自身の操作（作成・削除）は lane を待たず command 自身の
  結果で反映する。
- **順序は daemon の lifecycle revision で調停する**。lane の観測が利用者の command より前に始まって後に届いた場合、
  revision が古いので破棄する。どの lane が観測したかに関わらず最新の daemon 状態が勝つ。
- **cold-start の権限は session lane だけが持つ**。observation lane（decision / metrics）は起動中の daemon へ attach
  するだけで、`bootstrap.lock` の取得も lifecycle subprocess の起動も readiness 待ちも行わない。session lane は attach に
  失敗したときだけ cold-start へ落ち、その回数は workspace あたり 3 回に bound する。いずれも背景 thread 上で起き、
  描画スレッドが subprocess や sleep を実行することはない。
- **tick と resize は inventory を触らない**。terminal の wake-up（tick）と端末リサイズはどちらも再描画の機会であって
  観測の機会ではない。frame loop はこの 2 つを別のキーとして受け取り、どちらでも lane を起こさないため、ウィンドウの
  ドラッグリサイズは 1 event につき 1 回の再描画だけを費やす。実サイズは frame 先頭の `term.size()` から読む。
- **lane が応答しなくても frame は進む**。lane が hung / 不在でも frame loop は drain が空振りするだけなので、描画・
  入力・modal・quit は待たされない。
- **失敗の表示は失敗の連続に対して 1 回である**。cadence ごとに notice を積まない。decision lane は失敗状態へ入った
  ときに 1 回だけ notice を出し、次に成功したらその抑止を解く。session lane の失敗は refresh を要求した完了経路の
  notice として 1 回だけ出る。metrics lane の失敗は直前の sample を保持して mascot をちらつかせない。

## frame 予算

frame loop は 16ms（約 62Hz）で回る。[背景観測 lane](#home-frame-loop-と背景観測-lane) が daemon への同期 request を
frame から追い出したのに続き、**ローカルのファイル IO と全画面の再構築も frame 予算から外す**。この節が、1 tick が
何を払い何を払わないかの正本である。

| 作業 | idle な tick で払うか | 決めるもの |
|---|---|---|
| lane の drain（decision / session / metrics / terminal / pane completion） | 払う | 毎 tick 無条件 |
| restore retry の admission、pane launch の投入、入力処理 | 払う | 毎 tick 無条件 |
| `.usagi/sessions` のディレクトリ走査 | 払わない | inline create フォームが開いているか |
| frame の構築と端末への diff | 変化した tick だけ払う | frame material が前 frame と異なるか |

**描画だけを skip する。** drain と admission は毎 tick 走るため、skip された tick でも lane の観測は取り込まれ、
restore の再試行は期限どおり始まり、キー入力は同じ tick で処理される。skip は入力から反映までの latency を増やさない。

### create フォームの衝突ヒント

inline の `+ new session` フォームは、名前が既存の worktree と衝突することを daemon への往復より前に伝える。
その材料は `<workspace>/.usagi/sessions` の 1 回の `read_dir` である。

- 走査は**フォームが開いている間だけ**行う。閉じている間はディレクトリに触れず、ヒントは空である。
- フォームを開いた frame で 1 回走査し、開いたままなら**最大 500ms に 1 回**まで再走査する。
- 閉じるとヒントを捨てるため、開き直しは cadence 窓の内側でも必ず走査し直す。
- ヒントは best-effort である。開いている間に他 client が作った worktree を取りこぼしても、**作成を拒否する権威は
  daemon 側**にあり、送信時に改めて検査される。

### frame material と再描画の判定

再描画するかどうかは、**renderer の入力（frame material）を前 frame のものと比較して**決める。「何かが変わった」を
イベントごとに手で立てる dirty flag 方式は取らない。material が等しければ frame も等しいという等式が成り立つのは、
**renderer が material 以外の値を読まない**からである。この規約のために、`render_home` は実時計を自分で読むのを
やめて呼び出し側から受け取る。renderer に新しい入力を足すときは material にも足す。

| 面 | material |
|---|---|
| Home | 端末サイズ、`HomeProjection`（reducer state・session 行・metrics・git 差分・live terminal 出力・pane tab・overlay modal・create pending）、quit 確認、create 失敗 dialog、秒単位に丸めた現在時刻 |
| Welcome / Open / New / Config | 端末サイズと、その画面のフォーム |

**時刻も material である**。sidebar の session 行が出す相対時刻（`now` / `3m ago`）は実時計に依存するので、時計を
material に含めないと idle な Home で表示が止まる。丸め単位は 1 秒で、相対時刻の最小粒度（分）より細かいため遅れは
生じず、時計だけを理由とする再描画は最大でも毎秒 1 回である。entry 画面が受け取る時刻は起動時に固定した値なので
material ではない。

entry 画面は時間駆動の要素を持たないため、idle な tick は**一切描画しない**。Home には
[mascot](#sidebar-mascot) の瞬きがある。うさぎは 6 tick 周期だが見た目は 3 種類（休み・瞬き・耳）しかないため、
material は同じ見た目の tick を 1 つに畳む。瞬きの間隔は畳む前と変わらず、idle な Home の再描画は約半分になる。
除去や pending tab のように毎 tick 動くアニメーションが画面にある間は畳まず、従来どおり毎 tick 描画する。

modal（workspace config、config の save wave）は frame loop の外から端末へ直接描くため、戻った直後の tick は
material にかかわらず必ず描き直す。

## Session sidebar rows

Home sidebar は `session* → + new session` の順序と stable session identity を保つ。作成 action は
1 行、各 session は固定 2 行で描画する。`main` 行・root divider・`Sessions` 見出しは表示しない。session が
0 件なら `+ new session` が唯一の selectable row となる。作成中の skeleton は `+ new session` の直前に置く。session の 1 行目は cursor / active marker、表示名、常に幅を
予約する note icon に加え、daemon projection に assignment がある場合だけ `[role-id]` badge を描く。badge は表示専用で、attach / remove の可否は従来どおり lifecycle capability だけから決める。
予約する note icon を表示する。note icon は既存の text overlay を開く入力を増やさず、内容の有無だけを示す。

2 行目は daemon snapshot の `last_active`、または旧 record の `created_at` を基準に、`now`、`12m ago`、`3h ago`
のような相対時刻で表示し、dismissed でない PR があれば先頭の PR 番号と残り件数を続ける。Git の検査が完了した session は、remote の既定 branch（`origin/HEAD`）を優先した base との差分として `↑ahead ↓behind + added - removed` を続ける。base branch 名は表示しない。追加数は緑、削除数は赤で描く。相対時刻・commit 差分・追加数・削除数は、表示中の全 session で共有する固定幅の列に配置する。検査は sidebar の描画とは別スレッドで行い、完了後は 1 秒以上あけて現在の session 集合を再検査する。未完了・取得不能・意味を持たない base branch 自身の状態は表示しない。PR title の解決はこの行の前提にしない。snapshot に無い
session は selected / active を surviving session、または `+ new session` / active なしへ縮退させる。空一覧でも作成 action は残る。作成に失敗した `failed` session は
Danger 表現で `failed` タグとその失敗理由（daemon の安全な `failure.summary`）を 2 行目に表示し、使用可能な行と区別する。

Switch で `+ new session` を選び Enter（または `t`）を押すと、その行が `+ new: <name>` の
inline 入力欄へ置き換わる。置き換わった入力欄でも `+ new` affordance は静的な `+ new session` と同じ
Success（緑）で描き、入力中の名前と block caret は白で描く。新規 session 作成の affordance、入力欄、作成中
skeleton には Accent（青）を使わないため、静的行から入力欄へ移っても affordance の緑が途切れない。
入力欄はその行が入力を所有しているため、選択を表す `>` chevron は描かず、空のマーカー列で affordance を静的
ラベルと揃える。名前を入力して Enter を押すと通常の `session create <name>` と同じ daemon
request を非同期に開始し、完了まで行の直前に session と同じ 2 行の skeleton を表示する。skeleton の activity glyph と session 名は Success（緑）で同じ
左から右へ流れる低速の wave で描き、静的な点滅にはしない。daemon が同一 `OperationId` と revision を持つ `session.created`
完了 hook を返したときだけ、skeleton をその response 内の snapshot row に置き換えて loading を終了する。IME に依存しない `Ctrl-A` も
同じ inline 入力を開く。`Ctrl-A` は選択カーソルも `+ new session` 行へ移動する。Esc は入力を取り消す。作成は名前と read-only role picker を受け取り、profile / model は指定せず daemon の workspace default policy に委ねる。picker は effective session-scope catalog の default を初期選択し、↑↓ / Tab で候補を切り替え、daemon へは role ID だけを送る。catalog が不正なら picker を空に縮退させる。入力中は英数字・`-`・`_` 以外、64 文字超過、または daemon snapshot で表示中の session と、read-only に検出した `.usagi/sessions/` の既存 worktree と同じ名前を caret 行の下に error として表示し、空の名前は Enter 時に error を表示する。未マージ branch の安全な削除に失敗した session も `failed` 行として snapshot に残るため、その branch が所有する名前には入力中から `session name already exists` を表示する。error は caret 行と同じ 1 行に詰めて末尾を切り捨てるのではなく、sidebar 幅（`unicode-width` 準拠の表示桁数）に合わせて caret 行の**下へ折り返して**表示するため、CJK を含む長い安全文でも切れずに読める。折り返した行数は `+ new session` 行の高さ計上（viewport の scroll 起点と footer）と一致させ、error が伸びてもレイアウトがずれない。これらは local validation で daemon へ送る前に弾き、入力（draft）は失わないので、error を直して再送できる。local validation の error（入力に付随）と、daemon が受付後に作成を拒否したときの表示は別物として扱う。前者は入力欄の直下に出し、後者は下記の作成失敗 dialog で安全な message だけを提示する。
作成 request の受付後、完了まで入力がなければ、作成された session を選択して Closeup へ移る。完了前に入力があればこの自動遷移を取り消し、
作成完了後もその時点の操作 surface を保つ。
完了 snapshot は sidebar row と daemon-issued session ID を同時に置換するため、`a` のような短い名前も
表示名ではなく stable ID で後続の Agent / terminal request を送る。snapshot の schema が不正な場合は raw
IPC body を画面やログへ出さず、安全な error を画面に表示して `<data dir>/logs/error-YYYY-MM-DD.log` に schema
error を記録する。

daemon が受け付けた作成 request がその後に失敗したときは、Home 背景を残す confirmation/dialog style の
error modal で安全なメッセージを提示する。表示するのは安全化した safe message だけで、raw protocol /
internal / secret detail は画面に出さない（daemon の stderr は先頭 1 行だけを安全に採り、multi-line や
verbose な detail は漏らさない）。その safe message は dialog 幅に合わせて折り返し、途中で切り捨てず全文を
表示する（box は行数に合わせて伸びる）。折り返しは左の 2 桁 indent と同じ幅を右にも確保するため、枠いっぱいに
折り返した行でも枠の左右の内側余白は対称に保たれる。この dialog は skeleton・pending row を片付けたうえで開くため、
`Enter` / `Esc` / `Ctrl+C` で閉じると Home（Switch）へ戻り、作成入力や中途半端な作成状態を残さない。作成
フォームなど別の overlay が前面にある間に失敗が届いた場合は、その overlay を壊さず従来どおり notice へ退避する。
これは入力段階の inline validation（未受付の名前を行の下に error 表示する挙動）とは別で、dialog は受付後の
daemon 失敗だけを扱う。

session create / remove / refresh の admission は workspace ごとに容量 1 とし、queue は持たない。先行 command の
worker が daemon port を所有している間に届いた 2 件目は backend action を開始せず、create なら要求 token に対応する
失敗 `OperationResult`、remove / refresh なら安全な Busy notice を即座に 1 件返す。したがって 2 件目の skeleton や
pending overlay は残らず、実行順序・queue cancel policy は発生しない。worker panic は安全な失敗 completion に変換して
admission を回復し、遅延・順序外の worker completion は command 世代で fence して現在の pending 表示を上書きしない。
workspace を離れた場合は実行中の daemon operation 自体を取り消さず、worker は completion 経路を 1 回完了して終了する。
閉じた workspace の receiver はその completion を破棄し、次に開く workspace は factory から fresh port を取得する。

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
`config` は引数を取らず、現在開いている workspace の Config を Agent / Issue / Memory だけの overlay modal で開く。
`garden` は引数を取らず、[session garden](#session-garden) を手動で開く。
`roles [workspace|global]` は versioned `roles.toml` の source editor を開く。Ctrl-S は effective catalog として検証して atomic 保存し、validation error は source draft を失わず inline 表示する。Tab は layer を切り替えて保存済み source を読み直す。14 行の表示窓は ↑ / ↓ で 1 行、PageUp / PageDown で 1 ページ移動し、読み込み時と末尾への追記時は source の末尾へ自動追従する。
Global Config で保存した Modal mode は、次に開く Overview / Closeup から新しい選択方式が反映される。Issue / Memory の
MCP公開設定は [MCP server の設定反映](07-mcp.md#tool-面) に従い、MCP再接続後に反映される。

`session create <name>`、`session list`、`session overview`、`session resume <name>`、`session remove <name> [--force]` は
Overview の実行 port を通じて daemon IPC request になる。この実行 port は起動経路に依存せず、
Welcome→Open・Welcome の Recent・direct な Workspace entry のいずれで開いた workspace でも同じ
daemon-authoritative な port を通る。screen graph は workspace 起動ごとに port を新しく生成し、
daemon の snapshot revision を workspace 間で持ち越さない。remove の target は command の name に限定し、
現在選択中の session record や root を暗黙に使わない。daemon が request を受理できない場合は、
modal に安全な error を表示する。

Overview の `daemon` は workspace の daemon status modal を開く。modal は最新の metrics observation と
daemon-authoritative な session projection を使い、health、CPU / memory、接続 client 数、managed session の
running / waiting / failed 件数、Agent concurrency の使用中 / 上限、workspace 全体の Agent runtime inventory を
一画面に表示する。runtime は root または stable `SessionId` で結合した session label、状態、表示専用の短縮 runtime ID を持つ。
modal を開くたびに既存の coalesced restore lane へ新しい coherent inventory を要求し、取得までは待機表示にする。
一覧が表示域を超える場合は収まる先頭行と残件数を表示する。値は診断専用であり、
launch admission や ownership の判断には使わない。metrics 未取得と Agent concurrency 未報告はそれぞれ
`waiting for daemon observation` / `unreported` と表示し、Agent inventory の初回取得前も待機表示にして idle と読み替えない。

live Agent の枠は対象 tab で `Ctrl-D` により Agent を終了して解放する。status modal 自体は読み取り専用で、Esc で閉じる。

`session resume <name>` はその session の provider conversation を明示的に再開する。TUI は新しい
`OperationId` で pending Agent tab を作り、daemon が返す新しい完全な `TerminalRef` だけを同じ pending tab へ
昇格する。この pending pane を伴う経路は controller の `ResumeAgent` effect だけが所有し、他の session command
port は resume を受理しない。provider-native ID は受け取らず表示もしない。live Agent、resume metadata の欠落、
scope/revision 不一致は安全な error として収束し、provider の last session や旧 PTY を推測しない。
sidebar は daemon snapshot の `available` session に加えて、名前を占有し続ける `failed` session も
`failed` の状態と失敗理由付きで表示する。`failed` 行は使用不可（`can_use=false`）なので新しい pane の launch を提示せず、
削除可能（`can_remove=true`）なので `x` / `X` の remove をそのまま受け付ける。ただし、その session に daemon 所有の
既存 pane tab が残っている場合だけ Enter で Closeup を開ける。これは Agent へ `Ctrl-D` を送り global slot を解放する
回収経路であり、session の scope や checkout を再び使用可能にはしない。各行の可否は snapshot の lifecycle
から client 側で導出する（`SessionLifecycle::capabilities` が正本）。`deleting` session も表示し、削除中の行
（Danger の `✂` と wave）として描く。daemon は remove を受理した時点で応答し、worktree の撤去は daemon 所有の
worker が続けるため（[5. daemon の session teardown worker](05-daemon.md#session-teardown-worker)）、この行は
撤去が終わるまで（巨大な `target/` では分オーダー）残り、完了で消える。`deleting` は使用不可かつ削除不可
（`can_use=false` / `can_remove=false`）なので、新しい pane の launch も再 remove も提示しない。既存 pane tab が残る間だけは
`failed` と同じ回収用 Closeup を開ける。この表示は daemon の lifecycle だけを
根拠にするため、削除を要求していない別の TUI や再起動後の TUI でも同じ行が削除中として見える。reservation 状態
（`creating` / `initializing`）は 1 request で完結し、作成中 skeleton という固有の表現を持つため一覧に出さない。
いずれの場合も attach 対象は広がらず、scope 解決は引き続き `available` だけを対象とする。

### env editor

Overview の `env [workspace|global]` と Closeup の `env` は Config と同じ複数行 textarea の環境変数 editor を開く。
Overview は引数なし（または `workspace`）でこの workspace のスコープ、`global` で全 workspace 共通のスコープを編集する。
Closeup は**引数を取らず workspace スコープだけ**を編集し、Workspace Config と同じ複数行 textarea を表示する。
`env global` など引数付きの Closeup 入力は editor を開かず安全な notice で拒否する。Closeup から開いた場合も
対象 session 固有の環境を作らず、この workspace に属する root / session の次回 pane 起動へ共通して効く。
保存場所・スコープの合成・secret の解決・注入は [9. 環境変数設定](09-env.md) が正本で、ここでは editor の
操作だけを述べる。

Global / Workspace Config は Theme や Modal mode と同じラベル列にある `Env  [ N variables ]` を選んで Enter を押すと、
その Config scope の editor を開く。
開く直前に対象 scope の最新 binding を読み、`NAME=value` を 1 行 1 binding とする複数行 textarea に表示する。
textarea は背景色で入力領域を示し、空行には placeholder を表示しない。Enter は textarea 内で改行する。
Global Config は `Ctrl-S`、Workspace Config は `Tab` で Save action へ移動して Enter を押すと、編集中 scope の
`env` だけを全置換保存する。Global Config の `Tab` は何もしない。
行を取り除くと binding を削除できる。Esc は未保存の environment draft を破棄して元の画面に戻る。

Config、Overview、Closeup textarea の入力は次のとおりである。

| 入力 | 動作 |
|---|---|
| 文字 / `Backspace` / `←→` | textarea を編集する |
| `↑↓` | 前後の行へ caret を移動する。短い行をまたいでも元の列を保持する |
| `Delete` / `Home` / `End`（Config） | caret の位置または端を基準に編集する |
| `Enter`（textarea） | 改行する |
| `Ctrl-S`（Global Config / Overview / Closeup） | 編集中の `env` を保存する |
| `Tab`（Workspace Config / Overview workspace / Closeup） | textarea と Save action の focus を切り替える |
| `Enter`（workspace editor の Save） | workspace `env` を保存する |
| `Esc` | editor を閉じる |

Overview と Closeup は Config と同じ背景色付き textarea と中央の Save action を表示する。
Overview の scope は開くコマンドで決まり editor 内では切り替えない。Overview workspace と Closeup は `Tab` で Save action へ移り、
Enter または `Ctrl-S` で workspace binding 全体を保存する。Overview global は Global Config と同様に `Tab` を受け付けず、
`Ctrl-S` で保存する。
Overview と Closeup は保存完了の `EnvironmentSaved` を受けると editor を閉じる。入力の検証エラーまたは保存エラー時は editor を開いたまま入力とエラーを保持する。

- Config、Overview、Closeup の workspace editor は workspace binding だけを表示する。global binding を編集可能と誤認させず、
  保存時も global settings を変更しない。
- 保存は差分ではなく編集中スコープの全置換で、消した変数は取り除かれる。相手スコープの binding は
  変わらない。
- 読み込み中と保存中はその状態を表示し、保存中の再保存・編集・スコープ切り替えは受け付けない
  （二重送信の防止）。
- 入力行が `NAME=value` の形でない、または名前が移植可能な識別子でない場合は、入力を保持したまま
  安全な error を表示する。読み込みや保存の失敗も editor に留まり、入力を失わずに再試行できる。
- Overview から開いた editor で `Tab` を押すと相手スコープを読み直す。切り替え前の未保存の編集は破棄される。

## session garden

session を庭の区画、その session に属する Agent runtime を区画内のうさぎとして眺める screen saver である。
Home の一時的な全幅レイヤーで、daemon 権威の lifecycle・最新の coherent Agent inventory・controller が runtime
ごとに保持する Agent phase だけを絵に写す。背面の route・active target・pane・terminal subscription は変えず、
閉じると表示前と同じ Home へ戻る。設計判断は
[15. session garden](proposals/15-session-garden.md) を参照する。

開き方は 2 つある。Overview の `garden` command で手動で開くか、Home が一定時間 idle になったときに
自動で開く。

### 区画とうさぎ

1 区画は 1 session、1 うさぎは 1 Agent runtime である。session の lifecycle は nameplate と区画の pose、Agent
phase は各うさぎの pose と状態内訳へ投影する。利用可能な session に runtime が無ければ `no agents` の空区画を
描く。runtime が 1 つなら従来と同じ大きなうさぎを描き、複数なら固定幅の区画に小さなうさぎを最大 3 羽並べる。

うさぎの membership と stable identity は、controller が既に phase を観測した runtime に加え、workspace open 時の
最新 coherent Agent inventory から補う。同じ `AgentRuntimeId` が両方にある場合は、`Waiting` まで区別できる
controller の runtime-local phase を優先する。inventory にだけある runtime は `reserved → ready`、`live → running`、
`interrupted` / `unavailable → interrupted`、`exited` / `reclaimed → done` に写す。workspace root の runtime と、
Home に存在しない session の runtime は区画へ加えない。

複数 runtime は `Waiting` を先頭にし、残りと同 phase の tie-break を stable `AgentRuntimeId` 順にする。4 羽目
以降は末尾から畳み、状態内訳の末尾へ `+N hidden` を表示する。このため入力待ちの runtime は低い注目度の runtime
より先に見える。`Waiting` 自体が 3 羽を超える場合は、描けない羽数を `+N wait hidden`（他 phase も隠れる場合は
`+N hidden (W wait)`）と明示する。状態内訳は `2 run · 1 wait` のように phase ごとの羽数を文字でも示す。
`Ended` / `Exited` は瞬きへ戻さず、`done` の静止 pose で描く。workspace root の runtime は session 区画に属さない
ため描かない。区画の幅と `SessionId` hitbox は羽数で変えない。選択中の区画は `>` と Accent の nameplate、
それ以外は dim の nameplate で区別する。`Failed` は daemon projection が安全化した短い failure summary だけを
`failed · <summary>` として幅内に表示し、raw error、path、provider-native ID は renderer へ渡さない。

`Running` は 3 pose、`Waiting` は `?` を保ったまま耳をゆっくり交互表示する。`Creating` / `Initializing` は
土中から現れる 2 pose、`Deleting` は位置を固定して段階的に dim にする。animation は既存 frame tick を共有し、
同じ pose を描く tick は canonical tick へ畳んで frame material の不要な再描画を抑える。
composition root は起動時に `USAGI_REDUCE_MOTION=1` を読み、boolean を projection へ注入する。この設定では
全 pose を静止姿勢に固定し、lifecycle と Agent phase の状態ラベルだけを更新する。

### 自動表示

**最終操作から 5 分**で開く。キー、paste、pointer の press と wheel、端末 copy、live pane への passthrough、
terminal resize が「操作」で、受けるたびに monotonic clock 上の最終操作時刻を更新する。frame tick、
daemon / backend event、Agent や terminal の出力は操作ではないので idle を延長しない。したがって Agent が
動き続けていても、人が操作していなければ Garden は開く。

次のいずれかが前面にある間は自動表示しない。未送信の入力や destructive action の確認を screen saver で
覆わないためである。

| 前面 | 自動表示 |
|---|---|
| overlay（確認 dialog・編集中の form・Overview / Closeup / Daemon などの surface） | しない |
| Director drawer | しない |
| overlay の無い Switch | する |
| overlay の無い Closeup（live terminal を含む） | する |

端末が 64 桁 × 14 行に満たない場合も開かない（操作できる一覧を screen saver で覆わない）。閾値の判定は
frame loop が monotonic time と user input を観測して経過時間を controller へ注入する形で行い、controller
自身は時計を持たない。

### 起こし方とクリック遷移

| 入力 | 挙動 |
|---|---|
| 任意の key / paste | 最初の入力を wake-up として消費して Home へ戻る。背面の terminal や form へは渡さない |
| terminal resize | Garden を閉じ、idle timer を測り直す |
| うさぎを single click | その plot に束縛した stable `SessionId` を選択・active にして Garden を閉じ、既存の Closeup へ入る。double click 待ちは無い |
| うさぎ以外を click | click を消費して Garden を閉じ、表示前の Home へ戻る |

click は frame を描いたのと同じ layout 関数が返す `SessionId` 付き rectangle に当てて解決する。controller は
画面座標から session 順を再計算しないため、CJK label・端末 resize・表示上限で click target がずれない。
click と同時に session が snapshot から消えていた場合は stale target を実行せず、Garden を閉じるだけにする。
うさぎの click は sidebar の activation と同じ経路を通るので、使用できない checkout（`failed`）は選択されるが
attach されない。Garden から daemon command は発行しない。

## PR modal と browser effect

workspace entry は各 `SessionId` の daemon PR snapshot を読み、dismissed でない先頭 PR と残件数を
sidebar の `PR #<number> +<count>` に投影する。`p` の PR modal は focused `SessionId` について同じ
projection を即時表示し、resident PR lane を wake する。sidebar projection は新しい revision だけで進み、
開き直した modal は同じ cache を即時利用する。session ごとの初回 snapshot は baseline として表示用 cache にだけ
保存し、後続 revision で新しい URL を初めて検知したときは、他の modal や Director drawer が前面にない場合に、
その session の PR modal を検知した行を選択して自動で開く。title / state だけの更新、重複・古い revision、
dismissed PR は自動表示せず、前面の操作を奪わない。別 session の値は対象 session の cache にだけ反映する。

resident PR lane は render thread の外で daemon との persistent connection を所有し、1 秒以下の bounded cadence で
現在の session 集合を観測する。session の追加・削除は集合を全置換して即時 wake し、結果は frame loop の non-blocking
drain から controller へ戻す。したがって daemon RPC の timeout が workspace の初回 frame や入力を止めることはない。
snapshot が取れない場合は安全な unavailable 表示に留まり、legacy workspace state
や TUI scanner を production の fallback にしない。Open、Closed、Merged、Dismissed と title を表示し、
dismissed を新規検出として通知しない。

snapshot の `refresh` は daemon-owned freshness metadata である。`pending` は初回取得待ち、`idle` は freshness
window 内の成功、`backing_off` は last-known title/state を表示したまま remote retry を待つ状態を表す。TUI は
独自 timer、`gh` 呼び出し、失敗時の空 snapshot への置換を行わない。refresh の間隔、dedupe、backoff、restart、
shutdown の正本は [daemon の PR refresh scheduler](05-daemon.md#pr-refresh-scheduler) とする。

Enter は選択中の canonical HTTPS PR URL を browser effect に 1 回渡す。合成ルートは macOS では
`open`、Linux では `xdg-open`、Windows では `cmd /C start "" <url>`（空文字は `start` が消費する
window title 引数）を argv として実行する。URL を shell command に補間せず、検証失敗、
未対応 platform、起動失敗は TUI を終了させず safe feedback にする。同じ browser effect は
[live terminal の URL クリック](#live-terminal-の出力表示と入力)でも再利用する。
Closeup の `close [-f|--force]` は、選択中 session の削除を Overview と同じ daemon session-command port へ
直接依頼し、`-f` と `--force` は同値である。target、未知 flag、重複 flag は安全に拒否する。

`session remove -s [--force]`（`--select` も同義）は、現在選択中の row を即時削除せず、中央の
session checklist を開く。`↑`/`↓` または `j`/`k` で cursor を移動し、Space で複数 row を選び、Enter で
選んだ session の削除を開始する。選択済み候補と `Enter: remove` action は Danger（赤）で描き、未選択候補と
`Esc: cancel` は破壊的でない表現を保つ。Esc は選択を捨てて元の Switch / Closeup surface に戻る。空一覧、未選択の
Enter、modal 表示中の背景入力は安全な no-op であり、追加の確認 step はない。modal は開いた snapshot の
`name`、`root`、`created_at` を entry の incarnation fence として保持する。refresh により一致しない entry は
request 前に除外するため、同名再作成や一覧更新で別の session を削除しない。

modal の共通枠は本文の上下左右に 1 セル分の内側余白を持つ。短い端末でも overlay は上下に Home 背景を1行ずつ残す。
modal は view ごとに予約した body 行数で描画する。候補数、empty state、result、error、loading、editor の
内容が変化しても、開いている modal の枠高さは変わらない。端末が短い場合は予約領域を安全に clip し、
Home 背景との合成範囲を越えない。

### 共通 body-composition kit

枠・配置の primitive（`boxed` / `render_modal` / `render_over` / `fixed_body` / `modal_inner_width`）の 1 段上に、
各 modal が共通で使う **body 組み立ての約束事**を `widgets/modal.rs` に集約する。view は「何を表示するか」だけを
持ち、余白・style・reserve は kit に委ねる。

| helper | 生成する行 |
|---|---|
| `content_line(text, inner)` | body の 2 桁インデント + 内側幅への clip |
| `caption(text)` | dim の見出し・注記行（2 桁インデント） |
| `heading(text)` | accent 太字の見出し行（editor / 詳細 modal） |
| `empty_notice(text)` | dim の空状態行（`(none)` / `no pull requests` など） |
| `footer(hints)` | dim の help / フッタ行 |
| `selection_marker(selected)` | 選択行の danger 太字 `›`（`widgets/select.rs` と同一経路） |
| `scroll_above(n)` / `scroll_below(n)` | dim の scroll indicator `↑ N more` / `↓ N more` |
| `render_body` / `render_body_over` | body 予約（`fixed_body`）＋中央配置／背景合成の双子。over は小端末で `height − 4` に clamp |
| `choice_buttons(selected, choices)` | 固定幅の選択ボタン列。label は共通幅へ pad するため focus で geometry が動かない |
| `render_choice_over(.., selected, ChoiceView)` | 3 択以上の prompt を背景へ合成する。footer は複数行を受ける |

インデント・footer 文言・選択マーカー・scroll 文言は 1 経路に統一する。移行では表示を byte 単位で回帰させない
ことを基本とし、次の 3 か所だけを意図的に統一した（対応する test を更新済み）。

- **text-viewer の scroll indicator**: 旧 `↑ N lines`（インデントなし）を、list modal と同じ `↑ N more`
  （2 桁インデント）へ揃えた。
- **Overview の action-mode footer**: インデントの無かった footer を、他の footer と同じ 2 桁インデントへ揃えた。
- **共通マーカー**: `›` は `selection_marker` の 1 経路に集約し、`widgets/select.rs` の focus カーソルも再利用する。

### 共通 confirmation component

Yes/No の確認は `widgets/modal.rs` の `render_confirmation_over` 1 経路で描く。表示内容は
`ConfirmationView` に集約し、`ConfirmationView::confirmation(title, inner_width, heading, message)` が
標準の既定（danger の confirm・warning の cancel・`[ yes ] [ no ]` ボタン・`Enter/y: yes   Esc/n: no
  ←→/Tab: choose` の footer）を組む。呼び出し側は公開フィールドで label・role・footer 文言を差し替え、
`.compact(hints)` で単一キー hint の button なし variant（focus トグルを持たない prompt 用）に切り替える。
footer 行は body-composition kit の `footer` helper を通す。

| 経路 | variant | footer hints |
|---|---|---|
| open の Unregister workspace | Yes/No ボタン（既定） | `Enter/y: yes   Esc/n: no   ←→/Tab: choose` |
| open の registry cleanup | compact（ボタンなし） | `y: remove   n/Esc: cancel` |

ボタン付き variant の Yes/No 選択状態は `ConfirmationModal` が持ち、compact variant は選択状態を
持たない（state 引数を読まない）。open の cleanup は list 本文に手組みしていた `y/n` prompt を廃し、
unregister と同じ overlay 経路で合成する。

Home の [exit prompt](#workspace-の離脱と終了) は 2 択ではないため `render_choice_over` を使うが、
ボタンの幅と focus 表示は `confirmation_buttons` と同じ `choice_buttons` を通るので、2 択と 3 択の
見た目は 1 つの規則から出る。

### 形別コンポーネント

body-composition kit の 1 段上に、modal を「形（shape）」ごとの薄い composition helper として整理する。
各 modal の view には固有の state・キー解釈・内容だけを残し、行の並べ方・scroll viewport・選択・prompt と
いった形の共通部分を `widgets/modal.rs` の shape helper へ寄せる。

| shape | 対象 modal | shape helper | 共通化する部分 |
|---|---|---|---|
| list | Prs / Closeup / Decisions（一覧・option） / remove | `list_window` + `scroll_window` + `selection_marker` | 選択追従の viewport・カーソルマーカー・`↑/↓ N more`・行 clip |
| text-viewer | Preview（`text_overlay`。PR error の Unavailable も） | `viewport_window` + `scroll_window` | offset 起点の読み取り専用 scroll・scroll indicator |
| editor | Notes / Environment / Decisions（editor） | `content_line` + `caption` / `heading` + `footer` | draft 行・section 切替・error 行・footer |
| palette | Overview / Closeup（prompt） | `prompt_line` + `subcommand_row` + list helper | `❯` 入力行・前方一致候補・inline subcommand picker・result / footer |

- **scroll viewport は 1 経路**。選択追従（list）は `list_window(len, selected, capacity)`、offset 起点
  （text-viewer）は `viewport_window(len, offset, capacity)` が半開区間 `[start, end)` を返し、`scroll_window(rows, start, end)`
  が `↑ N more` / `↓ N more` を挟んで窓を描く。pr_modal の旧 `visible_bounds` と text_overlay の inline scroll 計算は
  この 3 helper に統合した。
- **palette の入力行は `prompt_line(value, cursor)`**（danger `❯` + accent block caret）に集約し、Overview と
  Closeup（prompt）が同じ prompt を描く。inline subcommand picker は `subcommand_row(label, selected)` に寄せる。
  subcommand の quiet な `›` は list の danger カーソルとは別に保つ。
- **決定 modal の選択行は共通カーソルへ移行**した。旧 plain `>` を `selection_marker` の danger `›` に揃え、他の
  list modal と同じ `content_line(format!("{marker} {label}"), inner)` で描く。

## Sidebar mascot

Home の左 sidebar は footer の直上に usagi を表示する。frame は reducer が所有する tick でだけ
進み、瞬きと耳の動きは純粋 render で決まる。mascot block の直下には常に 1 行の空行を予約し、footer、
session viewport、pending row と重ならない。狭いペインでは menu の viewport を優先して mascot block 全体を
省略する。この tick が idle な Home の再描画をどれだけ発生させるかは [frame 予算](#frame-予算) が決める。

presentation が表示安全な message を供給した場合だけ、mascot の上に黄色太字の角丸 speech bubble を出す。
bubble は `╰─┬─╯` の tail を mascot の頭へ向け、Unicode 表示幅で折り返し、各行を sidebar 幅に clip する。
message が無いときは無言の mascot のままで、renderer はダミー文言を生成しない。bubble と mascot は装飾であり、
入力 focus や terminal tab の input owner を取得しない。modal は Home frame の上に合成されるため、mascot は背景の
一部として残る。

message の既定の供給元は**正常系以外の daemon 状態**である。[feedback](#feedback-と終了) が daemon 切断・再同期要求・
操作エラー・端末エラーのいずれかのとき、その安全な要約を 2 行の bubble にして bottom-left のうさぎに出し、一目で
異常に気づけるようにする。健全な正常系（feedback 無し・進行中 progress・再接続完了）はうさぎを無言に保つ。error ID を
含む詳細は footer の feedback 行に委ね、bubble には載せない。呼び出し側が明示 message を与えた場合はそちらを優先する。

mascot の右には最大 3 行の観測 status（sidecar）を並べる。sidecar は rabbit 自身の行に重ねて描くため、
mascot block の予約行数を増やさず session viewport の容量を奪わない。各行は rabbit の幅に揃えた同じ列から
始まり、sidebar 幅に合わせて clip する。現在の供給元は上から
[daemon health indicator](#daemon-health-indicator)（異常時だけ）、[session 状態別件数](#session-状態別件数)、
[Agent concurrency](#agent-concurrency)、daemon metrics（CPU / resident memory）の 4 つで、mascot block ごと
省略される狭幅ではいずれも表示しない。後ろ 2 つは同じ metrics snapshot から来るため、snapshot が無ければ
どちらも出ない。

**供給元は 4 つだが枠は 3 行しかない**。4 つとも語ることがあるときは **Agent concurrency 行が譲る**
（異常な daemon の方が急を要する報せであり、常設の件数・CPU/memory 行の位置も動かさないため）。この取捨は
合成側で明示的に行う。widget の上限に任せると、代わりに最下段の CPU / memory 行が無言で消えるからである。

### daemon health indicator

sidecar の最上段に、**異常または要注意のときだけ** daemon health の 1 行を出す。**正常なら行を出さない**ので、
正常時の Home frame は indicator 導入前と同一である。3 行が揃っても mascot の予約行数は変わらない。

health は**診断専用の projection** である。reducer state に載らず、Effect を生まず、command の可否・resource
ownership・fence の判定には一切参加しない。材料は表示専用の daemon metrics
（[4. daemon IPC](04-ipc.md)）だけで、永続化もしない。daemon 切断・再同期要求は
[feedback](#feedback-と終了) と mascot の speech bubble が正本であり、health はそれを二重に報告しない。
health が担うのは「観測できているか」と counter 由来の劣化だけである。

| 判定 | 条件 | 表示 |
|---|---|---|
| （静か） | 一度も観測していない（daemon 不在・lane 未起動）、または劣化していない | 行を出さない |
| daemon 無応答 | 観測済みで、最新 sample が 30s 以上古い | danger |
| metrics 停滞 | 観測済みで、最新 sample が 6s 以上古い | warning |
| 端末出力の欠落 | retention window から捨てた量が 1MiB/s 以上を 3 sample 連続 | warning |
| 端末出力の滞留 | PTY reader が queue 待ちした量が 256KiB/s 以上を 3 sample 連続 | warning |
| PR 検出の欠落 | PR projection queue の満杯で確定済み出力を scan せず捨てた | warning |
| 更新の取りこぼし | metrics tick の取りこぼしが毎秒 1 件以上を 3 sample 連続 | warning |
| background worker 停止 | 長寿命 maintenance worker のいずれかが panic した | danger |

daemon metrics の counter は process の生存期間で単調増加するため、値や「一度でも増えたか」で判定すると
**一度警告したら消えない indicator** になる。加えて端末出力の欠落は bounded retention window の通常動作で、
忙しい agent では常時増える。そのため判定は次の 4 つを守る。

- **rate で見る**。sample 間の差分を経過時間で割った毎秒レートを閾値と比べる。単発のバーストは通らない。
- **連続で見る**。閾値超えが規定 sample 数続いて初めて点灯する（PR 検出の欠落は通常動作でないため 1 回で点灯する）。
- **減衰する**。点灯は最後の該当 sample から 10s で消えるため、事象が止まれば indicator も消える。
- **再 baseline する**。counter の後退（daemon 再起動）、sample 時刻の後退、schema の変化、5s を超える観測の
  空白（再接続直後）では差分を取らない。したがって再接続の 1 発目が警告になることはない。

観測の停滞（freshness）は counter 由来の理由より優先する。停滞した snapshot から計算したレートを表示しない。
worker 停止は daemon restart まで回復しないため、process-local failure count が 1 以上の間は継続して点灯する。
metrics lane の失敗中も直前 sample が保持されるため（[背景観測 lane](#home-frame-loop-と背景観測-lane)）、
「観測が止まった」ことは sample の時刻でしか判定できない。判定は sample 列と現在時刻だけの純関数であり、
現在時刻は renderer の入力（[frame material](#frame-material-と再描画の判定)）として渡す。

表示は `⚠` と短い理由だけで、raw な端末出力・path・secret・error ID を載せない。理由は閉じた語彙から選ぶため、
自由文が indicator に流れ込む経路そのものが無い。**Nerd Font glyph は使わない**（`⚠` は speech bubble と同じ
BMP 記号）。狭い sidebar では文言を落として記号だけに縮退し、記号も置けない幅では行を出さない。

### session 状態別件数

sidecar の常設行として、表示中 session の **running / waiting / failed 件数**を出す。件数は daemon 権威の
projection から毎フレーム導出する派生値であり、[metrics](04-ipc.md) schema にも TUI の reducer state にも
別の情報源を作らない。導出の入力は 2 つとも既に Home へ届いているものを使う。

| 入力 | 権威 |
|---|---|
| session ごとの lifecycle | daemon の session lifecycle snapshot |
| session scope の Agent phase 集約 | daemon の Agent phase 報告（[interrupted Agent の tab 投影](#interrupted-agent-の-tab-投影と明示-resume)と同じ projection） |

分類は既存語彙だけで決め、新しい状態語彙を増やさない。1 session はちょうど 1 クラスに属するため、3 つの
件数の合計が session 数を超えない。

| クラス | 条件 | 色 |
|---|---|---|
| `fail` | lifecycle が `failed` | Danger |
| `wait` | `fail` でなく、phase 集約が `waiting` | Warning |
| `run` | `fail` でなく、phase 集約が `running` | Success |
| （非計上） | 上記以外（`absent` / `ready` / `done`） | — |

- 優先順位は `fail` > `wait` > `run` である。`failed` 行に古い phase 報告が残っている 1 フレームでも二重計上しない。
- **`failed` は lifecycle だけが権威**である。`ended` / `exited` / `interrupted` は phase 集約の `done` へ畳まれて
  非計上となり、失敗としては数えない。`interrupted` は daemon 再起動後に runtime identity を証明できなかった
  daemon 所有の projection 状態で、resume 可能な履歴であるため（[interrupted Agent の tab 投影](#interrupted-agent-の-tab-投影と明示-resume)）、
  使用不能な checkout を意味する `failed` とは別の事実である。
- **0 件のクラスは描かない**。3 つとも 0（session が 0 件の場合を含む）なら行自体を出さない。
- **daemon metrics が無くても出る**。件数は metrics observation と独立に導出するため、metrics 未取得でも
  sidecar にこの行だけが載る。
- 表示専用であり、click や絞り込みの操作は持たない。

### Agent concurrency

sidecar の常設行として、daemon が Agent launch を admit する際の **使用中/上限**を出す。値は daemon の admission
権威が報告した [agent concurrency projection](04-ipc.md#agent-concurrency-projection) そのものであり、TUI は
runtime を数え直さず、上限の定数も持たない。対象は Agent runtime の pool だけで、generic terminal capacity や
supervisor run の同時実行数とは別物である（正本は
[5. daemon](05-daemon.md#agent-concurrency-projection)）。

| 状態 | 表示 | 色 |
|---|---|---|
| 使用中あり（上限未満） | `3/16` | 上限の 3/4 未満は dim、3/4 以上は Warning |
| 上限到達（次の Agent launch は拒否される） | `16/16` | Danger |
| idle と報告された | `0/16` | dim |
| daemon が報告しない（metrics schema 3 より前の peer） | `—` | dim |

- **`0/16` と `—` を描き分ける**。前者は「daemon が idle と報告した」であり、後者は「daemon が何も言っていない」
  である。報告されない level を 0 と描くと、枠が空いているという誤った断定になる。
- 表示専用であり、この行から launch の可否を判断しない。次の launch を拒否するかは daemon が admit の瞬間に決める。
- 狭幅では他の mascot 行と同じ幅へ clip し、うさぎ自体が入らない幅では mascot block ごと省略される。
- **sidecar の 3 行が埋まるときはこの行が譲る**。[daemon health indicator](#daemon-health-indicator) が点灯し、
  かつ [session 状態別件数](#session-状態別件数)と daemon metrics も出ている場合、この行だけを落として
  health / 件数 / CPU・memory の 3 行を残す。異常時の frame は indicator 導入時と同一に保たれる。

## Closeup pane

Closeup pane の tab state は target-scoped registry が正本である。workspace root と各 session は同じ
registry API の別 entry を持ち、entry は pending、live tab、stable selection、forced action modal state を
所有する。session の切替は entry を破棄しないため、session A の create / completion / exit / close は session B
の tab、選択、modal state を変えない。background target の event はその entry だけを還元し、表示中 target の
attach や Closeup 遷移を発生させない。

Closeup tab は pending operation、live `TerminalRef`、または terminal を持たない完了済み document を持つ。pending completion は同じ
`OperationId` にだけ対応し、terminal live tab は完全な `TerminalRef`、完了済み document tab は operation で識別する。表示中 target の選択中 live tab だけを
attach し、選択外または background target の tab は background のまま保持する。

右ペインは session 名の右に tab を Chrome 風の chip として描き、その直下に active marker を置く。chip の表示順・label は表示専用であり、選択は pending / document の `OperationId` または terminal live の完全な `TerminalRef` から投影する。
幅が狭い場合も ANSI を閉じた上で chip を clipping する。pending chip は固定幅のまま tab 名の文字ごとに
低速の highlight wave を流す。
tab が無い target は、灰色の静的うさぎと `No tabs stirring yet. Enter starts one.` の案内を、それぞれ
右ペイン幅の中央に表示する。描画前に clip して各灰色 SGR を reset で閉じるため、狭幅でも後続の
画面へ色が漏れない。この空状態は tick や runtime 接続に依存しない。overlay はこの Home frame を背景のまま合成する。

Closeup action modal の表示と input owner は target entry の tab 有無と forced action state から導く。ここでの
「tab 有無」は pending・live・document のいずれの tab も 1 枚として数える（live pane の有無ではない）ため、起動待ちの
pending tab がある間は action modal を自動表示せず、その wave を覆わない。Switch で
`Ctrl-O Ctrl-A` を実行した場合は、選択 target の Closeup action を開いて modal に input を渡す。tab が 1 枚も無い
Closeup は action modal が management input を所有し、Enter で `agent` / `terminal` を確定できる。tab が 1 つ以上で
forced state が無い Closeup は tab が input を所有し、action modal は自動表示しない。tab があるときに action modal
を再び出すのは `Ctrl-O Ctrl-A` だけである。action modal が前面にある間の `Esc` / `Ctrl-C` は、tab の有無や forced
表示か base surface かに依らず、modal を閉じて Switch へ戻る（`Ctrl-O Ctrl-O` と同じ着地で、action picker を dead-end に
しない）。modal が所有する間、tab selection、close、terminal passthrough は dispatch しない。

Closeup action で `agent`、`terminal`、または `diff` を確定すると、同じ pending tab を即座に一覧へ表示する。completion まで
入力がなければ completion はその tab を選択して live / document tab に置換し、入力があれば自動選択を取り消す。この focus は
session 作成と同じ interaction gate であり、受付時の interaction count を completion 時と照合して一致したときだけ steal する
（読んでいる画面から focus を奪わない）。diff は terminal identity を持たない
document tab として完了し、安全な document 本文を tab の content area に描画する。session の `terminal` は daemon が stable session / worktree scope を解決して起動する
`login-shell` であり、TUI はローカル PTY を生成しない。session が利用可能でない、または daemon が応答しない場合は
pending tab を安全な feedback に置き換える。`←` / `→`（または `h` / `l`）と `Ctrl-O Ctrl-N` / `Ctrl-O Ctrl-P` は tab を巡回し、`Ctrl-O [` / `Ctrl-O ]` は
選択 tab を前後へ並べ替える。`Ctrl-O x` / `Ctrl-O Ctrl-X` は generic Terminal / document tab と、daemon へ未送信の
client-owned pending launch を閉じる。close 後は次の tab（末尾なら直前）を stable identity で選択し、最後の tab を
閉じたときだけ target selection と Closeup action の空状態へ戻る。generic Terminal の close は client subscription を
detach するだけで daemon-owned terminal を停止しない。pending launch は送信済み operation を推測して再送・cancel しない。

live / interrupted Agent tab は daemon inventory に存在する限り常に表示し、`Ctrl-O x` / `Ctrl-O Ctrl-X` では閉じない。
この操作には `Agent tabs stay visible; exit the Agent with Ctrl-D` を表示する。Agent runtime を終了して実行枠を空ける操作は、
対象 tab を選択して CLI へ `Ctrl-D` を送る。Closeup に Agent を非表示化・再表示するコマンドは持たせない。

shell が attach するのは、現在の active target に属する selected foreground terminal だけである。target / tab の
切替時は以前の subscription を detach する。background target と選択外 tab の terminal coordinator は bounded
cache にだけ保持し、foreground stream lane からは外す。
**定常状態の観測のために 1 frame が同期 request を出すことはない**: foreground の出力取得も background tab の exit 観測も
[背景 observation lane](#背景-observation-lane) が別 thread で行い、描画スレッドはその結果を非ブロッキングに drain するだけである
（利用者の操作が起こす attach / input / resize / detach は従来どおり描画スレッドから同期送信する）。
どちらの lane が exit を報告した tab も自動で閉じる。最後の live tab が exit したとき、tab が 1 枚も残らなければ
Closeup の action 空状態へ戻る（interrupted history などの非 live tab が残っている場合は tab surface に留まる）。
描画スレッドから同期送信する attach / input / resize / detach も含め、request ごとの実効 deadline は
[4. daemon IPC#terminal lane の per-request budget](04-ipc.md#terminal-lane-の-per-request-budget) が正本である。

### pane launch の command worker と常駐 stream の分離

Agent / terminal の launch は session create と同じく worker で実行する。ただし **launch command の client と常駐する terminal
stream の client は別物**である。worker は共有された launch client を借りるだけで、live pane の subscription・poll・input・
resize・detach を担う stream client には触れない。したがって launch が遅い・応答しない・panic する間も既存 pane の IO と TUI の
入力・終了はそのまま続き、focus 中のキーストロークが busy で失われることはない。pending chip は request を受け付けたフレームから
completion まで既存の共有 shimmer wave を表示し続ける。completion が到着した後の次フレームでは、request 受付後に入力が
なかった場合だけ同じ stable identity の live / document tab を選択する。

launch の admission と失敗の収束は次の規則で bound する。Agent / generic terminal、workspace root / session、foreground /
background のいずれも同じ規則に従う。

| 事象 | 規則 |
|---|---|
| 同時 launch | launch client を使う worker は同時に 1 件だけで、残りは可視の pending tab として queue に並び completion 後に着手する |
| queue 上限の超過 | daemon へ送らず、その 1 pane を typed Busy completion で即座に完了させる（pending tab を無制限に積み上げない） |
| worker panic | catch して同じ pane を safe failure completion へ収束させる。client は借用されただけなので失われず、次の launch は通る |
| completion channel の消失（workspace exit） | worker の送信は無害に捨てられ、stream client も launch client も worker と一緒に失われない |
| late / 重複 / 未 admit の completion | launch fence が一致した completion だけが admission slot を解放し、operation fence が一致した pane だけを完了させる |

hung request 自体の deadline は本節の責務ではなく、[#521](../.usagi/issues/521-fix-ipc-clientpolicy-request-deadline-reconnect-budget.md)
の IPC request deadline が所有する。

#### 同一 process の pending operation identity

pending pane と daemon の答えを結ぶ identity は、**controller が pending tab のために発行した 1 つの `OperationId`**
だけである。この節はその同一 process 内の規則の正本で、wire 側の identity と digest の契約は
[4. daemon IPC#agent operation identity と final の相関](04-ipc.md#agent-operation-identity-と-final-の相関)（Agent）と
[4. daemon IPC#generic terminal request](04-ipc.md#generic-terminal-request)（generic Terminal）が正本である。

| 段 | 規則 |
|---|---|
| controller → worker | pending tab を作った `OperationId` をそのまま launch request に載せる。Agent / generic terminal、workspace root / session で同じ |
| worker → daemon client | client adapter は別の `OperationId` を発行せず、その identity をそのまま daemon へ送る。Agent の答えは request と同じ identity・同じ intent digest・request scope に属する `TerminalRef` を検証してから返し、generic Terminal は同じ identity を [#518 の launch operation 契約](04-ipc.md#generic-terminal-request)へ渡す |
| daemon の答え → pending | その identity の pending tab がこの process に残っている間だけ完了させる。close 済み・置換済みの tab は復活させない |
| 検証に落ちた答え | pane を成功させず safe failure に収束させる（別 operation の side effect を pending へ promote しない） |

response が失われた・timeout した場合は、その pane を安全な失敗として閉じるだけで、**別の `OperationId` で blind retry
しない**。TUI を開き直したときの in-flight launch の replay や `OperationId` の再利用も行わず、復元は
[workspace open 時の pane 復元](#workspace-open-時の-pane-復元) の inventory 観測だけが決める。

### live terminal の出力表示と入力

選択中の foreground live terminal tab は、daemon が所有する PTY の出力を右ペインへ描画し、キー入力をその PTY へ
そのまま送る。TUI が使う同期 IPC client は push される stream event を受け取れないため、出力は **poll** で
取得する: foreground 化したときに一度 attach して daemon の **semantic screen checkpoint** と output offset を
受け取り、以降は redraw ごとに `Resume { after_offset }` で offset 以降の出力だけを取得する。attach では
checkpoint から screen を復元し（履歴の control byte を再生しない）、以降の suffix を**その復元済み parser**へ
feed する。screen は最小の VT screen（印字・
`CR` / `LF` / `BS` / `HT`・行折返し・カーソル移動・行/画面消去・scroll region を含む画面スクロール・SGR の色と属性・alternate screen buffer）で、
その screen 行を右ペインへ clip して表示する。PTY output の適用は parser state だけを更新し、retained scrollback 全体の
描画 cache は作らない。各 frame は現在の viewport に必要な行 window だけを ANSI 付き表示へ投影し、URL 検出もその
window に接する折返し logical line までに限定する。このため通常の output・idle redraw・scroll 操作は 10,000 行の
scrollback 全体を文字列化・全走査せず、表示行数と必要な折返し範囲に比例する。selection / copy は利用者が明示的に
開始したときだけ untrimmed な retained cells 全体を snapshot し、ドラッグ中の出力から選択対象を固定する。選択 highlight が
残っている frame も全 snapshot を再投影せず、通常表示と同じ viewport window だけを ANSI 付き表示へ投影する。
live の input cursor は現在セルを反転して表示する。output offset に gap があるとき、または daemon が
resync を要求したときは local に継ぎ足さず、daemon の atomic snapshot（再 attach）で置き換えて、その後の出力取得を継続する。

checkpoint は `output_offset` 時点の完全な screen state（可視 grid・scrollback とその oldest-row origin・cursor・saved cursor・
scroll region・SGR・alternate と背景 primary buffer・decoder の途中状態）を含むため、retention の先頭が
UTF-8 / CSI / OSC / SGR / alternate の途中でも reconnect 前後で可視セル・cursor・style が一致し、
`cells_with_scrollback` を使う selection / copy history も untrimmed な参照と一致する。

`Resume`（poll）は**描画スレッドでは行わない**。専用接続を持つ背景スレッド（foreground poll pump）が、attach 済みの
terminal を fetch して per-terminal の read-ahead バッファへ積み、描画スレッドは redraw ごとにそのバッファを**非ブロッキングに
drain** するだけである。daemon が一時的に応答できない間（例: dispatch 中に agent lock を保持している間）に固まるのは背景
スレッドの fetch だけで、描画・入力ループは即座に応答を続ける。attach で得た output offset を pump に登録し、再 attach
（reconnect / resync）では新しい snapshot offset で登録し直してバッファと fetch offset をリセットする。`Resume` は daemon 側で
接続にも subscription にも紐づかない stateless な操作なので、この専用接続の破棄・再接続は input の subscription・exactly-once
ledger・input sequence に影響しない。`Resize` は attach / input とは別の deadline 付き接続で送る（低頻度なので描画スレッドから
同期送信でよい）。失敗した geometry 同期は 100ms から 2s 上限の指数 backoff で再試行し、frame tick ごとに socket を
開き直さない。要求 geometry は pending として保持し、daemon の ACK までは decoded local screen を最後に同期済みの
geometry から破壊的に縮めない。成功した resize だけが local screen の geometry を確定し、foreground poll pump を
interactive cadence へ wake する。attach / input / detach は
従来どおり単一接続に載せる。この共有接続の epoch と subscription 無効化は
[connection epoch と subscription 無効化](#connection-epoch-と-subscription-無効化) が正本である。

#### stream 失敗の回復

**exit 以外の stream 失敗は、すべて同じ再 attach で回復する**。attach は subscription を取り直し、daemon の
atomic checkpoint から screen を組み直し、daemon 自身の `next_input_seq` を採用するため、失った cursor・
古くなった `TerminalRef`・input ordering のずれのいずれに対しても回復手段はこの 1 つである。したがって pane は
失敗の種類で運命を分けず、100ms から 2s 上限の指数 backoff で再 attach を続ける。
foreground poll worker の fetch が panic した場合も worker を終了させず、途中まで使用した owner 接続を破棄してから
一過性の接続失敗として同じ再 attach に流す。次の fetch は新しい owner 接続を確立する。
これにより daemon・Agent・PTY が動作中なのに観測 thread だけが消え、最後の screen が永久に静止する状態を作らない。

| 状態 | いつ入るか | 次に何が起きるか |
|---|---|---|
| live | attach 済みで streaming 中 | 出力を適用し、入力を PTY へ送る |
| 再接続待ち | exit 以外の attach / `Resume` 失敗（daemon 不通・resync 要求・stale な参照・ownership 不明・input ordering のずれ） | backoff 満了ごとに再 attach する。復帰するまで最後の screen を静止表示し、入力は安全な理由付きで拒否する |
| detach 済み | foreground を別の pane（例: [指示モード](#指示モードdirector-mode)の root conversation）へ渡した | 自分からは再 attach しない。再び foreground に選ばれた frame で attach する |
| exit 済み | process の終了を観測した | 最終 screen を保持し、tab は inventory 観測とともに閉じる |

detach と失敗の区別は状態名ではなく**予約された再試行の有無**であり、背景へ回した pane が勝手に attach を
奪い返すことも、失敗した pane が TUI の再起動まで回復しないこともない。daemon が拒否し続ける失敗
（二度と受理されない `TerminalRef` など）も同じ backoff で再試行し続ける。上限に達した backoff で
attach 1 往復 / 2s、しかも attach 済みの foreground pane 1 つ分に限られるため、「自力で戻れない pane を
表示し続けない」ことを優先する。

再 attach で live へ戻った pane は、失敗の種類にかかわらず reconnect として扱い、`Reconnected` feedback と
PR target の再同期を 1 度だけ発行する。

入力の順序は fence ではなく**待機 queue が空であること**が所有する。fence が外れたあとの drain が失敗で
中断されると queue だけが残るため、その状態で受け取った keystroke は PTY へ直接書かず queue の末尾へ入れ、
pane が live へ戻った frame で古い順に送る。

#### 背景 observation lane

daemon の出力・exit を観測する lane は 2 本あり、どちらも描画スレッドの外で、専用接続と bounded cadence を持つ。
Home の inventory（decision / session / metrics）を観測する 3 lane は
[Home frame loop と背景観測 lane](#home-frame-loop-と背景観測-lane) が正本で、この 2 本とは観測対象も cadence も別である。

| lane | 観測対象 | primitive | cadence |
|---|---|---|---|
| foreground poll pump | 選択中の attach 済み terminal 1 件の出力 | `Resume { after_offset }` | 出力がある間は interactive（8ms）。無出力が続くと 64ms 上限まで倍々に後退し、出力・attach・入力・resize で即座に interactive へ戻る |
| background inventory pump | detach 済み background tab の **exit metadata だけ** | scope 単位の `Inventory` | 2s。失敗中は 500ms から 8s 上限の指数 backoff |

この分離により、idle な TUI が生む daemon request は frame rate（約 62.5Hz）ではなく上表の cadence で決まり、pane 数にも比例しない
（foreground は常に高々 1 件、background は tab 数ではなく **scope 数**に比例する）。

- background lane は `Attach` も terminal 単位の `Resume` も**送らない**。detach 済み tab の観測 primitive は scope inventory だけである。
- background で bound するのは exit metadata の観測時刻（cadence + queue 遅延 + request deadline 1 回分）だけであり、**final output byte の取得時刻は bound しない**。
  final output は tab を foreground 化して再 attach したとき、または [completed entry](#exited-terminal-の-completed-entry) を history から明示選択したときに
  read-only に読む。
- inventory が `live: false` として列挙した tracked terminal だけを exit として扱う。reply から単に欠落している entry は exit とみなさず、
  次の観測へ持ち越す（partial / 誤 routing な inventory で tab を閉じないため）。要求した scope 外の entry を含む reply は失敗として backoff する。
- 各 lane は完了を **exact `TerminalRef` / scope + connection epoch + 要求時の cursor / watch generation** で fence する。focus 切替、resync、
  [epoch 変化](#connection-epoch-と-subscription-無効化)、tab の開閉で in-flight だった応答は新しい cursor へ適用せず捨てる。
- 1 つの ref / scope につき in-flight request は高々 1 件で、遅い owner に対しては request を積まず round を coalesce する。read-ahead バッファ、
  watch する scope / terminal 数、exit queue、1 frame あたりの exit 適用数はいずれも bounded である。バッファ上限超過は resync 要求へ変換する。
- lane が劣化した（fetch 失敗、overflow resync、inventory 失敗、scope 不一致、queue / watch の drop）ときだけ、workspace を閉じるときに
  各 lane の counter を [failure log](05-daemon.md#failure-logging) へ 1 行記録する。描画スレッドの外で完了する lane の失敗は UI に出ないため、
  これが後から追跡できる唯一の痕跡である。

#### connection epoch と subscription 無効化

attach / input / detach は**全 pane が 1 本の persistent connection を共有する**。daemon は connection が終わる
とその connection の attachment をすべて解放するため、wire の subscription id だけでは attachment を特定できない。
client は subscription を `{wire id, connection epoch}` として session に結び付け、epoch は**共有 transport を
破棄した時点で**進める（次に開いた時点ではない。破棄と再接続の間に古い subscription を current と誤認しないため）。

| 失敗 | 共有 connection | epoch | 他 pane への影響 |
|---|---|---|---|
| 完全に受信した protocol error（`resync_required` / `stale_target` など）、decode できない `Ok` body、非終端の `Accepted` | 保持する | 変わらない | 無い。当該 pane だけが resync / typed feedback へ進み、他 pane は subscription と ledger を保つ |
| transport 破断（EOF、frame 破損、write 失敗） | 破棄する | 進む | 全 pane の subscription が同時に無効になる |
| resize lane・[foreground poll pump・background inventory pump](#背景-observation-lane) の失敗 | 触らない | 変わらない | 無い。当該 lane だけを開き直し、attachment も input ledger も無効化しない |

epoch が進んだ session は、`Resume` も `Input` も送る前に **fresh attach** を行う。したがって recovery 後の最初の
打鍵は新しい subscription で一度だけ書かれ、解放済み attachment に対する effect-zero 拒否で失われない。
無効化された subscription の `Input` は接続を開く前に client 内で拒否するため、effect は確定して 0 である。
[background inventory pump](#背景-observation-lane) も同じ epoch で fence する。epoch が進むと in-flight の観測結果を捨て、
新しい epoch が使えるようになった時点から exit metadata の観測上限を測り直す。

detach は次の 2 つを local no-op として扱う。どちらも現在の connection と、他 pane が持つ attachment を変えない。

| local no-op にする detach | 理由 |
|---|---|
| old epoch の subscription | daemon はその attachment を connection とともに解放済みで、現在の connection には存在しない subscription の解放を要求することになる |
| 同じ terminal の新しい attach に置き換えられた superseded subscription | 新しい attachment とその出力登録（poll pump）は置き換え後の subscription が所有する |

epoch は attach ごとに新しい resource を作らず、client-local な transport の incarnation を数えるだけである。
replacement terminal を spawn することはない。どの endpoint へ送るかは epoch ではなく request が持つ
`TerminalRef.daemon_generation` が決め、connection と cursor は generation ごとに独立して保持する
（[4. IPC の owner generation routing](04-ipc.md#owner-generation-routing)）。

epoch は lane 集合全体を数える。1 つの lane を失ったときに全 subscription を無効化するのは、無効化しすぎても
再 attach で済む一方、無効化が足りないと daemon が既に解放した attachment で input を fence してしまうためである。
generation が 1 つのときは、この 2 つは同じ意味になる。

#### snapshot negotiation と legacy 限定表示

TUI は checkpoint 経路を **capability と negotiated revision の両方**で判定する（wire 契約の正本は
[4. daemon IPC#snapshot payload と revision](04-ipc.md#snapshot-payload-と-revision)）。

| daemon | client が使う経路 | 表示 |
|---|---|---|
| `terminal.screen-checkpoint.v1` を広告し共通 revision が 2 | checkpoint から復元し、suffix を feed | 履歴を含む通常表示 |
| capability 不在、または共通 revision が 1 | **legacy raw tail を parser へ流さない** | 履歴復元不可の限定表示。空の screen から `output_offset` 以降の live 出力だけを描画し、footer に履歴が復元できない旨を表示する |

任意の byte 境界で切られた raw tail は UTF-8 / CSI / OSC の途中から始まり得るため、限定表示では tail を
**一切 decode しない**（escape を文字として露出させない）。capability を真実源とするので、revision だけが
2 に見えても capability を広告しない daemon は限定表示へ fail closed する。

#### 要求 geometry と実効 geometry

pane が要求する geometry と、terminal が実際に取っている geometry は別物である。**同じ terminal は
複数の TUI window から同時に attach され、PTY は attach 中の全 window の要求の最小値を取る**
（正本は [5. daemon#共有 viewport（複数 client の geometry）](05-daemon.md#共有-viewport複数-client-の-geometry)）。
したがって client は次のように扱う。

| 値 | 何か | 使い道 |
|---|---|---|
| 要求 geometry | 自分の pane の viewport | attach request に載せて宣言し、以後は pane のサイズが変わったときだけ `Resize` で送る。最小値に負けても frame ごとに再送しない |
| 実効 geometry | attach snapshot と resize 応答が返す daemon 権威の geometry | local screen を組む幅・高さ。pane より小さければ余りは空白のまま描く |

実効 geometry が要求より**大きい**場合は pane に収まらないため、同期 fence を解いて次の redraw で
自分の viewport を再要求する。実効 geometry が要求と異なる間は「他の window と共有中でその viewport は
`cols`x`rows`」を footer feedback に出す（他の失敗表示があればそちらを優先する）。

#### revision fence

復元は revision fence を通す。old / new state を混在させず、失敗した snapshot は表示しない。

| fence | 条件 | 挙動 |
|---|---|---|
| revision | snapshot の terminal `revision` が既に適用した revision より小さい（stale snapshot） | その snapshot を破棄して subscription を外し、同一 attach 内で 1 度だけ atomic snapshot を再取得する。なお古いなら typed resync（`Reconnecting` + backoff）へ落とし、直前の screen をそのまま残す |

checkpoint の geometry は fence の対象ではなく、**採用する**。daemon が単一 PTY の権威であり、
自分の要求と違う geometry の snapshot を拒否すると、小さい window と共有した瞬間に再 attach を
繰り返しながら壊れた幅で描き続けることになるためである。bound 違反で reject された checkpoint
（未知 schema version・範囲外 geometry など）は同じ経路で fail closed する。resize が daemon へ
届かなかった attach も daemon 権威の geometry で復元し、viewport 同期の失敗だけを feedback に
表示する（attach 可能な terminal を隠さない）。

terminal pane の接続状態と footer feedback は `TerminalSession` の状態をそのまま投影する。

| 状態 | 入力 | poll / retry UX |
|---|---|---|
| `Live` | subscription と input sequence で送信。subscription が[無効化された epoch](#connection-epoch-と-subscription-無効化) のものなら、送信前に fresh attach する | output offset から継続取得する。epoch が無効化されている場合は `Resume` の前に fresh attach する |
| `Reconnecting` | 新しい入力は typed failure として拒否する。直前の ACK を失った場合はその入力を未配送と断定せず effect unknown を表示する | 100ms から始まり 2s を上限とする指数 backoff 後、同じ `TerminalRef` を attach して snapshot resync する |
| `Disconnected` | typed failure として拒否 | stale target または明示的 detach の終端で、自動 retry しない |
| `Orphaned` | typed failure として拒否 | ownership unknown の終端で、自動 retry しない |
| `Exited` | typed failure として拒否 | 最終画面を保持し、自動 retry しない |

一時的な `unavailable`、input effect unknown、および
[revision fence](#revision-fence) が拒否した snapshot が `Reconnecting` へ遷移する。再 attach 成功時は backoff をresetし、新しい
connection-owned subscriptionを使う。input sequenceはclient-local connection epochが変わった場合だけ0へresetし、
同じepoch上のcursor-gap/resync/detach→reattachではdaemon ledgerに合わせてnext sequenceを保持する。
tab close / detach は予約済み retry を取り消す。
retry 中に replacement terminal を spawn せず、stale / orphaned / exited を一時切断として再試行しない。

primary screen から押し出された行は 10,000 行を上限とする local scrollback として保持し、right pane は live bottom を基準に
表示する。alternate screen のスクロールは現在の full-screen frame の一部であり、過去 frame を scrollback へ混在させない。ホイール上/下でそれぞれ古い出力方向／live bottom 方向へ 1 行移動する。新しい
snapshot で履歴が短くなった場合は offset を有効範囲へ正規化する。`↑` / `↓` は scrollback 操作に予約せず、PTY の
history navigation へそのまま送る。right pane の footer の直前には常に 1 行の空白を置く。

**live bottom から離れている間、表示中の行は Agent の追記で動かない**。viewport が live bottom
（offset 0）にある間だけ新しい出力へ追従し、遡っている間は追記された行数を offset へ足し戻して同じ
retained 行を描き続ける。live Agent は読んでいる最中も出力するため、この保持がないと 1 行遡るたびに
窓が同じだけ前へ滑り、履歴の同じ位置に留まれない（[指示モード](#指示モードdirector-mode)の root Agent で顕著）。
live bottom へ戻すと追従を再開する。各 primary / alternate buffer は oldest retained row の
monotonic origin を持ち、viewport は row count と origin の両方から追記数を算出する。したがって 10,000 行上限や
daemon の cell / checkpoint frame budget で oldest row の eviction と追記が同時に起き、retained row count が
変わらない場合も同じ surviving content を保持する。buffer が切り替わった場合は別の座標系として扱い、
一方の origin を他方の追記量へ混ぜない。保持している間は live bottom までの距離が会話とともに伸びるため、
`Ctrl-O b` / `Ctrl-O End`（ScrollBottom）が 1 手で live bottom へ戻して追従を再開する。

出力は mouse drag により選択でき、drag 開始時の press cell から終点までを含めて、drag を離すと選択した ANSI を含まない表示テキストを OS clipboard にコピーする。drag 中も
drag を離した後も、選択範囲は右ペインに reverse-video で示し続ける。選択は右ペイン content 内の通常左クリック、次の drag が
新しい選択を始めるか、その terminal が論理 close / bounded cache eviction されるまで terminal identity ごとに保持する。
別 terminal へ focus が移った間は非表示になり、focus が戻ると scroll offset・selection・feedback を復元する（release で即座に消えない）。保持中の選択は OS 標準の copy shortcut（macOS: Command+C、Linux: Ctrl+Shift+C、Windows: Ctrl+C）で再コピーできる。この click は text selection
だけを解除し、sidebar の navigation / activation、modal の入力所有、PTY への入力を変えない。選択の可視化は選択した桁全体に及び、行末の空白 padding や
選択範囲に含まれる空行も反転する（agent が描く空白 padding 中心の画面でも選択が消えない）。各 OS の copy shortcut 以外のキー入力は
コピーに使わず、`Ctrl-C` を含めて live terminal へそのまま送る。
clipboard adapter は macOS の `pbcopy`、Windows の `clip.exe`、
Wayland の `wl-copy`、X11 の `xclip` / `xsel` を現在の環境に応じて使う。利用可能な backend がない場合は copy を成功扱いにせず、
安全な feedback を表示する。

出力中の `http(s)` URL は左クリックで OS 既定ブラウザに開ける。URL が載るセルは下線で装飾し、クリック可能で
あることを示す。drag で非空の選択が成立した release は**コピー**、選択が生じない素のクリックだけを**リンクオープン**として扱い、
両者は排他である（選択中はリンクを開かない）。クリック位置のセルを保持中の行/列へ写し、行末で折り返した URL は 1 本に結合して
開く。URL 上でないセルのクリックはブラウザを開かず、通常クリックとして保持中の選択だけを解除し、scrollback offset は変えない。検出・検証は純粋コアが担い（`http(s)`
スキームのみ許可し、制御文字・ESC・空白・非 ASCII を拒否する）、起動直前にも再検証してから argv で spawn するため、ANSI/端末制御
列がブラウザ引数へ渡らない。起動は [PR modal と browser effect](#pr-modal-と-browser-effect) と同じ browser effect（macOS `open` /
Linux `xdg-open` / Windows `cmd /C start "" <url>`）を使い、未対応 platform・起動失敗は TUI を乱さず safe feedback にする。
pointer の release は PTY へ入力として転送しない。

live terminal に focus がある間、leader が無い `Ctrl-C` / `Ctrl-Q` / `Ctrl-D` を除くすべての非 prefix キー入力（文字・修飾キー・paste・
raw bytes・Enter・Backspace・Tab・矢印など）は management ではなく PTY へ送られる。矢印は対応する CSI 列、Enter は `CR` に符号化する。端末では bracketed paste（DECSET 2004）を有効にし、複数行の貼り付けを 1 つの paste イベントとして受け取る。PTY へは bracketed paste マーカー（`ESC[200~` … `ESC[201~`）で包んで転送し、bracketed paste を要求している agent が埋め込まれた改行ごとに 1 行ずつ実行せず 1 ブロックとして挿入できるようにする（貼り付け内に含まれる終了マーカーは注入対策として除去する）。tab 巡回や Closeup/Switch の遷移は
`Ctrl-O` prefix（`Ctrl-O Ctrl-N` / `Ctrl-O Ctrl-P` / `Ctrl-O Ctrl-O`）だけが所有する。前面 modal や forced action modal がある間は
その modal が入力を所有する。入力は subscription と単調増加する input sequence で fence し、同じ打鍵を二重送信しない。
daemon の input ACK は `Written` だけを通常成功とする。`Failed` は 0 byte 適用を表示し、`Ambiguous` は
`applied_prefix` byte 適用後の effect が不確定であることを表示する。`Cached` は内側の outcome へ正規化する。
これら有効な ACK は非成功を含めて sequence を 1 つ進め、既存 subscription を保ったまま次の入力を送れる。

Live でない、subscription がない、または definitive な送信前 failure は success と扱わず、未配送を safe feedback として
footer に表示する。一方、request write 後に transport / ACK を失った入力は delivery が unknown であり、未配送と断定しない。
その入力を blind replay せず `Reconnecting` へ移り、利用者が不確定な command を認識できる feedback を残す。
未知の ACK variant、範囲外の `applied_prefix`、過剰に深い `Cached` も同じく fail closed とする。
ACK lossと`Ambiguous`のmutation uncertaintyはtransport recoveryや後続`Written`では消さず、複数件をcount + first/latestで
集約する。現行UIにはclear操作を置かず、session破棄と daemon の durable outcome resolution だけがこれを解く。後から
stale/orphaned/exited/resize failureが起きた場合はcurrent terminal errorを先に表示し、prior input uncertaintyも同じfooter
feedbackへ合成して隠さない。

ACK を失った入力は表示だけでなく、その pane の **ordering fence** になる。fence がある間の打鍵は PTY へ送らず生成順に
bounded queue（既定 64 件 / 8 KiB）へ保持し、footer は「順序を保って保留中」であることと待ち件数を示す。queue が満杯に
なった打鍵は typed backpressure として拒否し、黙って捨てたり順序を入れ替えたりしない。fence は次の redraw tick で
daemon へ**当該 operation の outcome 照会**を送って解消する（同じ bytes は再送しない）。`Written` なら uncertainty を
撤回して queue を順序どおり流し、`Failed` / `Ambiguous` なら outcome をそのまま投影してから流す。daemon が記録を
持たない `unknown` の場合は fence を latch し、以後は照会も再送も行わない。fresh connection epoch は
`input_seq` を 0 へ戻すが、未収束 operation と queue は保持する。identity・wire・ledger の bound は
[4. daemon IPC#terminal input identity と cross-connection replay](04-ipc.md#terminal-input-identity-と-cross-connection-replay) を正本とする。

terminal は起動時点と resize 後の右ペイン実幅・高さで geometry を要求するため、shell の right prompt も pane 内に収まる。geometry が変わると TUI は PTY と decoded local screen を resize する。過去の cursor 移動列は新しい幅で再生せず、過去行を含む既存セルを clip して行数を増やさない。daemon 不通・stale・orphan は安全な
feedback だけを表示し、local PTY を生成しない。

Closeup の `agent [-m <cli>]` は既存 session だけで実行できる。TUI は選択した CLI を product-neutral な
profile ID（`claude` / `codex` / `sakana-ai`）へ解決して durable operation に渡し、argv・model・secret は組み立てない。
TUI は daemon の accepted response 後に Agent pending tab を置き、同じ operation の成功 final が返す
完全な `TerminalRef` にだけ attach する。daemon 不通、拒否、未知・古い completion では local spawn や
名前からの terminal 推測をしない。

daemon inventory、attach/resume、stream、resync は `pane_runtime` が結合する。output cursor に gap が
ある場合は local output を継ぎ足さず、daemon の atomic snapshot で置き換える。resize は geometry の
変化時に送り、失敗した場合は同じ pending geometry を capped exponential backoff で再試行する。ACK 後にだけ
右ペインの VT screen を resize して PTY と同じ viewport に確定する。detach はこの client の
subscription を外すだけで、PTY を kill しない。daemon が exit を報告した terminal または Agent は、その
live tab と client subscription を直ちに外し、残る tab または Closeup の空状態へ戻る。

`agent [-m <cli>]` は active な session だけを対象にする。`-m` を省略した request は
[config の `default_model`](#settings-scope-と-workspace-entry) を解決して明示 profile として送る。controller が発行した
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
request retry、attach を行わない。failure は pending tab を除去し、daemon が安全と保証した文言を error modal
として表示するとともに `<data dir>/logs/error-YYYY-MM-DD.log` に記録する。確認して閉じると、tab-less
Closeup の action modal に戻る。

## Closeup の agent CLI 選択

Closeup の `agent` は `-m`（長形式 `--model`）で起動する agent CLI を選ぶ。この節が v2 の agent CLI 選択の正本である。

| 入力 | 起動する CLI | daemon profile |
|---|---|---|
| `agent` | config の `default_model` | 解決した CLI の profile |
| `agent -m claude` | Claude Code | `claude` |
| `agent -m codex` | Codex | `codex` |
| `agent -m sakana.ai` | sakana.ai（Codex 互換、実行は `codex-fugu`） | `sakana-ai` |

- **候補は install 済みの CLI だけ**である。合成ルートは起動時に provider CLI を実行せず PATH lookup だけで
  `AvailableModels` snapshot を一度作り、process lifetime を通して Config、Closeup、Director に同じ値を注入する。Action menu の
  展開行・Tab 補完・submit 時の検証はすべて同じ集合を使う。install されていない CLI は表示・補完せず、直接入力しても
  `that agent CLI is not installed` として拒否する（daemon へ request を送らない）。
- **default は config の `default_model`** である。Action menu の展開行は default の行に `(default)` を付ける。
  default の CLI が install されていない場合は `the configured agent CLI is not installed` として拒否する。
- daemon が CLI の未認証・readiness 不成立などで起動を拒否した場合は、daemon が返した安全な復旧理由を error modal に
  表示する。protocol rejection を接続失敗へ置き換えないため、`agent -m codex` では install・sign-in を確認して再試行
  すべきことを画面上で判断できる。
- **Tab 補完**は Prompt mode の入力欄と Action menu の filter で同じ文法を使う。`agent -m sak` → `agent -m sakana.ai`、
  `agent --` → `agent --model` のように候補が 1 つなら確定し、**候補が複数のときは Tab を押すたびに巡回する**
  （`agent -m c` → `agent -m claude` → `agent -m codex` → `agent -m claude`）。曖昧さで Tab が無反応になることはない。
  Action mode では `→` で `agent` 行を展開し、`↑↓` で `-m <cli>` を選ぶ。filter へ引数区切りを含む
  command line（`agent -m codex` など）を直接入力した場合も、その入力全体を submit する。
- daemon の同時実行枠が満杯の場合は `Agent slots full; exit one with Ctrl-D, retry` を表示する。すべての既存 Agent は
  tab に表示されるため、終了する Agent を選択して `Ctrl-D` を送ってから再試行する。Agent tab の close は非表示化しない。
- 位置引数（`agent codex`）も同じ語彙・同じ install 判定で受け付ける。`-m` の重複、値の欠落、複数選択、未知の flag は
  安全な文言で拒否し、modal を閉じない（拒否の文言は [Closeup 入力の拒否表示](#closeup-入力の拒否表示) が正本）。
- CLI 名の解決は大文字小文字を区別せず、`-` / `_` / `.` を同じ区切りとして扱う（`sakana.ai` / `sakana_ai` /
  `sakana-ai` / `codex-fugu` はすべて同じ CLI）。

## Closeup 入力の拒否表示

Closeup の command 入力（Prompt mode の入力欄・Action menu の確定）が拒否されたとき、その理由を modal の中に
表示する。この節が Closeup の拒否表示と Tab 補完の巡回の正本である。

拒否された submit は effect を 1 つも生まず overlay を閉じないため、**画面に理由が出ないと Enter が無反応の
キーと区別できない**。したがって modal は最後の拒否理由を 1 行の danger 行として持つ。

| 状況 | 画面 |
|---|---|
| submit が拒否された | modal は開いたまま、入力を保持し、拒否理由を danger 行で表示する |
| submit が受理された | overlay が閉じ、modal（と拒否理由）は破棄される。起動は pending tab の wave が示す |
| 入力を編集した | 拒否理由は消える（古い理由を残さない） |

- 表示位置は modal の body 内で、Prompt mode は入力欄の直下、Action mode は入力欄と action 一覧の間である。
  どちらも枠の高さを変えず、Action mode で picker を展開しても押し出されない。理由の文言は幅で clip する。
- 文言は reducer が持つ安全な文言そのもの（`unknown agent CLI` / `that agent CLI is not installed` /
  `the configured agent CLI is not installed` / `agent accepts one -m selection` / `-m requires an agent CLI name` /
  `unknown agent flag` / `unknown closeup command: "…"` / `invalid close arguments` /
  `env takes no arguments (usage: env)` など）で、host path・argv・認証情報は含まない。
- **Tab 補完は候補を巡回する**。候補が 1 つなら確定し、複数なら Tab を押すたびに次の候補へ進み、末尾で先頭へ戻る。
  巡回は Tab を押し始めた時点の入力を基準に測るため、Tab 自身の置き換えで基準を失わない。入力の編集・`↑↓`・
  `→`/`←` は巡回を終わらせ、次の Tab は新しい入力から数え直す。command 名の補完は選択中の行から始まるので、
  `↑↓` で選んだ command はそのまま Tab で確定できる。

## Closeup Agent の手動確認

Agent profile を利用できる daemon を起動し、既存 session を選択して Closeup を開く。次の操作は実装済みの
runtime bridge を確認する手順である。profile の install 状態、認証内容、argv は画面に入力・表示しない。

| 操作 | 確認結果 |
| --- | --- |
| Action menu の Agent、または `agent -m codex` を確定する | 同じ session の `Agent` tab が出て、wave が daemon の pending operation を示す |
| matching final を daemon が replay する | pending が Agent tab に一度だけ置換され、選択中なら attach される |
| Agent が stdout を出力する | 選択中 Agent tab の pane に出力が表示される |
| 選択中 Agent tab で入力し、端末を resize する | 入力は一度だけ daemon に届き、geometry 変更時の resize は成功するまで再試行される |
| daemon を切断して再接続する | process を作り直さず、inventory で検証済みの選択 tab だけが attach/resync される |
| profile 未準備・daemon 不通・Agent exit を発生させる | pending tab は消え、Closeup の action launcher が再度開く。起動が失敗した場合（Agent の正常終了と異なり）は、その safe reason が再度開いた launcher の notice として表示される |

## workspace open 時の pane 復元

daemon は terminal / Agent runtime の権威 owner であり、TUI を閉じても runtime は daemon 内で継続する。
そのため workspace を開き直した（同じ client の再 open、または 2 つ目の client の open）とき、root scope の
**live Agent** は[指示モード](#指示モードdirector-mode)、各 available session scope の
**live Agent / Terminal** は Closeup の pane tab に復元する。root scope の generic Terminal / Diff は復元しない。planned restart 中は active と draining の
両 generation が inventory に答え、完全な `TerminalRef` で merge / dedup した結果を投影する
（[4. IPC の owner generation routing](04-ipc.md#owner-generation-routing)）。

指示モードの live terminal 本文は Closeup 右ペインと同じ viewport component を使う。retained
scrollback 全行を受け取る選択中の frame でも、本文は live bottom を基準に `scroll` を反映した window だけを描き、
表示幅で clip する。copy・選択・link 操作の feedback は本文へ追加せず footer に表示し、feedback がある frame では
drawer の key hint より feedback を優先する。interrupted Agent の safe reason は terminal 本文ではなく専用の detail
行に表示する。

この inventory 復元は provider conversation resume を開始しない。managed session の `identity_unknown` /
interrupted Agent は sidebar の第 2 行、root scope の interrupted Agent は指示モードの選択中
conversation の専用 detail 行に、ID を含まない safe reason を表示する。TUI 起動、workspace open、daemon reconnect は
resume を自動送信せず、managed session は `session resume <name>`、root scope は drawer で選択した tab の
`Ctrl-O r` を必須とする。

- **two-source reconciliation**: daemon の unified terminal / Agent inventory が membership・liveness・PTY ownership の正本、
  `<data-dir>/tui/workspaces/<workspace-id>/agent-tabs.json` の `AgentTabIntent` が Agent tab の表示順・target ごとの選択の
  正本である。local state は workspace identity、完全な last-known `TerminalRef`、
  provider-neutral な `AgentContinuationRef` だけを持ち、provider ID、argv、environment、transcript、terminal output を
  保存しない。generic Terminal tab は従来どおり inventory だけから復元する。
- **タイミング**: 初回 frame を paint した後、UI event loop と別の daemon connection で
  [`terminal inventory`](04-ipc.md#generic-terminal-request) / `agent_inventory` / `terminal inventory` の順に取得する。
  前後の terminal snapshot が同一で、live Agent が両 inventory で一対一に対応する全量 observation だけを適用する。
  transport・partial・不整合は controller 所有の capped exponential backoff で再試行し、初回 frame・キー入力・animation を
  待たせない。失敗時は last valid intent を空 snapshot で上書きせず、generic tab だけを部分適用せず、local spawn もしない。
  成功後も dedicated restore port を保持する。restore socket の passive EOF を検知し、current endpoint が再び接続可能になった
  ときだけ monotonic / coalesced connection epoch を 1 件発行して、その epoch につき fresh observation を一度送る。frame tick
  自体は inventory RPC を発行しない。restore request が slow / hung でも off-thread worker 内に隔離されるが、request deadline
  そのものは [#521](../.usagi/issues/521-fix-ipc-clientpolicy-request-deadline-reconnect-budget.md) の責務である。
- **投影**: saved Agent は完全な `TerminalRef` が両 inventory で trusted live と確認できたときだけ保存順で復元する。
  inventory にだけある live Agent は continuation / terminal fence の決定的順序で末尾へ追加し、duplicate snapshot は
  exact ref で 1 枚へ収束する。managed session の generic Terminal はその後ろへ決定的に追加し、root scope の generic
  Terminal は拒否する。saved ref が non-live でも同じ continuation が resumable なら slot intent を保持し、
  interrupted tab として root drawer または managed-session Closeup へ投影する。表示中 surface の selected foreground
  tab だけを attach / resync し、background target と選択外 tab は detached のまま保持する。
- **遅延応答 fence**: restore dispatch 時の UI interaction count と pane-registry revision を結果に持たせる。双方が一致する
  結果だけが durable Observe と pane projection を適用できる。遅延・順序外の結果は全体を拒否し、専用 port が戻り次第、
  fresh fence で一度だけ再観測する。後続の reorder・selection を上書きせず focus を奪わない。transport failure と
  fence rejection が同時なら transport failure を優先して outage backoff を維持する。
- **commit**: Agent tab の確定した order / selection は file lock 下の atomic read-modify-write で commit する。
  revision CAS が競合した場合は最新 state を読み直して stable continuation key ごとに mutation を適用する。stale
  Observe は最新 exact ref を stale candidate へ置換せず fresh observation を要求する。保存失敗時は reorder / selection の
  可視 UI を変えず、typed safe notice を表示する。coherent restore の
  `Observe` 保存だけが失敗した場合も、既存 Agent / pending / generic の順序と選択を変更せず、inventory-only Agent を表示しない。
  generic inventory の新規 ref だけを append / exact-dedup し、次の保存成功 observation が全量 membership / order を確定する。
- **session removal**: 成功した lifecycle snapshot の available session 集合は target 存在について authoritative である。集合から
  消えた session は target の selection / slots を同一 commit で除去する。他 target は保持する。session が allowed のまま
  inventory から runtime だけが欠落した場合は dormant slot を保持し、
  history retention の根拠にしない。available 集合の変更自体が controller の coalesced observation を1件要求する。既存 outage の
  backoff は短絡せず、旧集合で in-flight の結果は session-set fence で拒否して fresh 集合を一度だけ再観測する。

誤復元・二重 tab を防ぐため、次を守る。

| 入力 | 判定 | 動作 |
|---|---|---|
| legacy dismissal が保存されている | coherent な全量 observation | dismissal を消去し、inventory に存在する live / interrupted Agent を表示する |
| saved exact ref が trusted live | unified terminal と Agent inventory の双方に完全一致 | 保存 slot へ live tab を 1 枚投影する |
| saved ref は non-live、同じ continuation は resumable | durable history は存在 | slot intent を保持し、interrupted tab は自動投影・resume しない |
| `live: false`（死んだ process / exited / orphan / identity_unknown） | attach 不可 | live tab を作らない。PTY master 復元不能は interrupted 契約に委ねる |
| authoritative に削除された session | 成功した lifecycle snapshot の available session 集合から消えた | target / selection / slots を同一 commit で除去する |
| allowed session 内の inventory 欠落 | session 自体は lifecycle snapshot に残る | dormant slot を保持し、別 target を復元する |
| scope mismatch（別 workspace / worktree / session） | daemon が scope 完全一致で filter | 列挙されない |
| saved generation が current active と異なるが、その owner generation が saved exact ref を live として列挙した | merged inventory が owner 自身の答えを持つ | 保存 slot へ live tab を 1 枚投影し、attach / input は owner endpoint へ配送する（[4. IPC の owner generation routing](04-ipc.md#owner-generation-routing)） |
| saved generation の owner が answer しなかった | partial inventory は absence ではない | last-known tab を `reconnecting` として保持し、interrupted へ変換しない |
| saved generation が trusted registry から消えた | verified retirement | tab を回収する。attach も input も別 generation へ再配送しない |
| exact-equal terminal row の重複 | `TerminalRef` / kind / live がすべて同じ | normalize して 1 row にする |
| conflicting terminal row / duplicate live continuation / Agent↔terminal 非全単射 | 同じ fenced ref の kind / live が競合する、または live Agent 対応が一意でない | observation 全体を拒否して retry する |
| daemon 不通 / partial / cross-RPC 不整合 inventory | coherent な全量 observation ではない | 全 pane restore を適用せず retry し、intent を変更せず local PTY も作らない |
| corrupt schema | current schema として読めない | private peer へ quarantine し、空 intent から安全に再構築する |
| future schema | current build より新しい | 元 bytes を保持して read-only にし、restore / mutation を適用せず typed notice を表示する |

### legacy dismissal の移行

既存 schema の `dismissed` / `dismissed_terminals` field は旧 build の保存データを読めるよう残すが、新しい close 操作は
どちらも書き込まない。最初の coherent な全量 observation は両 field を同じ commit で空にしてから reconcile し、以前に
非表示化された Agent を含めて daemon inventory に存在する live / interrupted Agent をすべて投影する。partial / 不整合な
inventory では移行を確定せず、次の coherent observation を待つ。

## resume data compatibility

旧 state の欠落は空の `AgentTabIntent` として読む。schema は versioned で、旧値から continuation や terminal を
推測する migration は行わない。TUI-local resume state が持てる terminal identity は完全な `TerminalRef`、Agent lineage
identity は daemon-issued `AgentContinuationRef` だけである。表示名、path、provider ID、単独の terminal ID から terminal
を探し直したり、新しい terminal を spawn したりしない。

| 復元時の入力 | 判定 | fallback |
|---|---|---|
| saved managed session target が snapshot に無い | target identity が stale | active pane にせず、Home は surviving session または `+ new session` へ reconcile する |
| saved `TerminalRef` が live inventory に無いが continuation history は残る | attach 不可 | slot を保持し live tab は投影しない |
| inventory から continuation が消えた | absence は retention / GC の証明ではない | slot を保持する。aggregate allocator / retention policy は [#526](../.usagi/issues/526-fix-daemon-terminal-agent-tombstone-retention-aggregate-bound-gc.md) の責務 |
| terminal ID が同じでも daemon generation など fencing field が異なる | old / stale data | trusted owner が無ければ attach せず、名前や ID から置換しない |
| attach / resync が ownership unknown または transport failure | 継続性を証明できない | safe feedback を表示し input を無効化する |

この migration は旧値を推測変換しない fail-closed policy である。TUI-local data は表示・選択の
復元候補に限られ、terminal、PTY、session mutation の所有権は daemon に残る。

## exited terminal の completed entry

live restore（[workspace open 時の pane 復元](#workspace-open-時の-pane-復元)）が `live` runtime だけを tab へ
戻すのに対し、TUI 不在中・一時切断中に exit した Agent / generic Terminal の
tombstone へは、fresh TUI が **completed / read-only entry** として到達する。daemon が tombstone を retain し、
[`completed_inventory`](04-ipc.md#exited-tombstone-visibility) で final replay locator・exit status・workspace-global
visibility を返す（daemon 側の正本は [5. daemon](05-daemon.md#terminal-ownership)）。接続中に exit したときも、
final output を保持したまま completed entry へ到達可能にし、即時 auto-close で final を到達不能にしない。

completed entry は **read-only** である。input / resize / live Resume / spawn を一切送らず、daemon の bounded final
replay を表示するだけである。live tab とは別 identity（完全な `TerminalRef`）で持ち、`live` restore の tab と衝突しない。

tombstone が到達可能である期間は daemon の aggregate retention が決める（正本は
[5. daemon](05-daemon.md#final-retention-と-aggregate-gc)）。final は observed / unobserved を問わず minimum
visibility TTL の間は保護されるので、その間に completed entry を表示して `observe` / `dismiss` できる。TTL 経過後の
final は pressure 下で決定的な順に回収され、`dismissed` が最初、まだ見ていない `unobserved` が最後になる。回収後の
history 選択は typed な expiry になり、別 terminal の history へ fallback しない。TUI は inventory から entry が消えたことを
retention の証明として扱わず、この typed 応答だけを根拠にする。

### workspace-global visibility の投影

visibility は client-local な「既読」flag ではなく、daemon が authority を持つ workspace-global state（`unobserved <
observed < dismissed` の lattice と revision）である。TUI は completed inventory を取得し、次の純粋 projection で
completed tab と visibility command を決める。projection は入力の純粋関数なので、CAS conflict を merge した後の
再取得でも同じ結果へ収束する。

| tombstone の visibility | history からの明示選択 | 投影 |
|---|---|---|
| `unobserved` | なし | completed tab を一度だけ自動表示し、`observe`（CAS）を送って「一度見た」を記録する |
| `observed` / `dismissed` | なし | 自動表示しない（再通知・二重 tab を作らない） |
| `unobserved` / `observed` / `dismissed` | あり | 該当 exact `TerminalRef` を read-only で表示する。`dismissed` は解除せず runtime も resume しない |

- `observe` / `dismiss` は expected revision を伴う CAS で送る。conflict は authoritative snapshot を返すので、
  それへ merge して再投影する。late / out-of-order な write は `dismissed` を下げず、completed tab を復活させない。
  process-local な「見た」flag を authority にしない。
- 別 exact `TerminalRef` incarnation の visibility は独立する。close / dismiss しても別 incarnation を誤抑止しない。

## interrupted Agent の tab 投影と明示 resume

daemon の crash / `SIGKILL` / cold stop-start / OS 再起動のあとでは旧 PTY は失われている。daemon は各 conversation
lineage を **interrupted runtime** と exact resume source として保持し、[`agent_inventory`](04-ipc.md#provider-conversation-resume-request)
で返す。TUI はこの inventory を、`live` restore（[workspace open 時の pane 復元](#workspace-open-時の-pane-復元)）とは
別の **interrupted tab** 群へ投影する純粋 reducer を持つ。projection は入力の純粋関数なので、refresh・reconnect・
重複 inventory でも同じ tab 集合へ収束し、二重 tab を作らない。

interrupted tab は live tab ではない。subscription、input、resize を一切持たず、保持する `TerminalRef` は
「もう attach してはいけない旧 incarnation」の識別と表示順のための値である。tab が持つ表示情報は closed vocabulary
（provider 種別・safe reason・safe phase）だけで、provider-native ID、argv、cwd、transcript、raw daemon error は
label・detail・feedback・log のいずれにも出さない。

### 投影規則

| inventory の入力 | 判定 | 投影 |
|---|---|---|
| runtime state = `interrupted`、scope が current workspace root または refresh 済み session 集合の内側 | 復帰候補 | continuation ごとに interrupted tab を 1 枚作り、root は drawer、managed session は Closeup へ投影する |
| 同じ continuation を `live` / `reserved` runtime が保持している | live が authority | interrupted tab を作らず live tab へ収束する |
| runtime state = `exited` / `reclaimed` | 完了 history | interrupted tab にしない（[completed entry](#exited-terminal-の-completed-entry) の責務） |
| 同じ continuation の interrupted record が複数 | resume 可能な 1 件が上位、次に exact ref 順 | 決定的に 1 枚へ畳む |
| exact-equal な inventory row の重複 | 同じ lineage | 1 枚に collapse する |
| `AgentRuntimeRef` の session と terminal の session が不一致 | scope を信頼できない | 投影しない |
| workspace / allowed session の外側、または別 workspace の inventory | scope mismatch | 投影しない |
| legacy dismissal が保存されている | coherent observation | dismissal を消去し、他の interrupted Agent と同じく表示する |
| resumable item が無い / `available: false` / reason が `explicit_resume_available` でない | metadata 不足・live 保持・supersede 済み | tab は表示するが resume 不可にし、safe reason だけを出す |
| target の lineage / runtime / workspace / session / worktree が当該 runtime と一致しない | 信頼できない target | target を捨て、resume 不可として表示する |

saved 表示順（[#506](../.usagi/issues/506-feat-tui-agent-tab-intent-daemon-inventory-open-reconcile.md) の slot 順）を
持つ lineage はその位置を保ち、local state に無い lineage だけが決定的な順序でその後に続く。したがって local state を
失っても inventory から安全に再構成でき、provider ID を推測しない。

### 明示 resume の検証

resume は利用者の明示操作だけが発火する。TUI 起動、workspace open、inventory refresh、daemon reconnect、
planned restart は resume request を作らない。要求は選択中の exact tab の `AgentResumeTarget` と新しい
`OperationId` だけを送り、応答は次を **すべて** 満たしたときにだけ同じ tab を live へ置き換える。

| 検証 | 不一致時 |
|---|---|
| 応答の operation が、その tab で in-flight な operation と一致する | stale な応答として無視する |
| daemon が source → replacement relation を返した | 確認できないため置換しない（通常 launch の応答では置換しない） |
| 応答の `AgentContinuationRef` が tab の lineage と一致する | 別 conversation として拒否する |
| relation の `source` が tab の target の source と一致し、replacement runtime が source runtime と別 incarnation である | 別 source / 再利用として拒否する |
| relation の `replacement_terminal` と応答 `TerminalRef` が完全一致し、旧 `TerminalRef` と異なり、tab の scope 内である | 新しい terminal を得られていないため拒否する |

同じ tab に対する重複操作（double click）は in-flight な operation へ収束し、2 つ目の request も spawn も作らない。
拒否・失敗は interrupted tab をそのまま残し、provider ID を含まない safe reason と retry 可否だけを表示する。
他の tab、他の history、selection、provider conversation、runtime record は変更しない。

### surface ごとの表示と操作

投影された interrupted tab は target ごとの pane registry entry に入り、root と managed session の history は
互いに混ざらない。root は[指示モード](#指示モードdirector-mode)の conversation selector、managed session は Closeup の live tab と
同じ tab strip に表示する。live restore は live membership だけを所有し、interrupted tab の membership・順序・
selection は projection だけが所有する。

cold restart 直後のように **interrupted tab しか無い target** でも、root drawer は conversation surface、
managed-session Closeup は action launcher ではなく tab strip へ着地する（[Closeup pane](#closeup-pane) の入力所有者は
live PTY の有無ではなく tab の有無で決まる）。history tab は managed-session Closeup では `Ctrl-O Ctrl-N` /
`Ctrl-O Ctrl-P`、root drawer では `Ctrl-O n` / `Ctrl-O Ctrl-P` で選び（drawer の `Ctrl-O Ctrl-N` は New）、
どちらも `Ctrl-O r` で resume できる。

| 状態 | tab label | 選択時の body |
|---|---|---|
| resume 可能 | `Claude (interrupted)` / `Codex (interrupted)`（metadata 無しは `Agent (interrupted)`） | `This conversation was interrupted. Resume starts a new Agent for it.` |
| resume 不可 | 同上 | 当該 [safe reason](#投影規則)（provider ID を含まない 1 行） |
| resume 中 | `Claude (resuming)` | `resuming this conversation` |

操作は次の順に進む。

1. `Ctrl-O r` が選択 tab の opaque `AgentResumeTarget` と新しい `OperationId` を daemon へ送り、**その tab だけ**を
   resume 中にする。tab の位置・selection・他 tab は変わらない。
2. 応答が[明示 resume の検証](#明示-resume-の検証)をすべて満たしたときだけ、同じ slot の tab を新しい exact
   `TerminalRef` の live Agent tab へ置き換える。foreground だった tab だけが attach / resync する。
3. 置換した lineage は #506 の slot intent へ新しい `TerminalRef` として commit するので、次の observation でも
   同じ位置に残る。
4. 拒否・失敗は tab を interrupted のまま残して safe feedback を出す。in-flight な operation を持つ tab は、
   inventory から source が消えても消滅しない（利用者の request が答えを受け取るまで tab が残る）。
5. `Ctrl-O x` は tab を閉じず、`Agent tabs stay visible; exit the Agent with Ctrl-D` を表示する。interrupted tab は
   runtime を持たないため `Ctrl-D` も送らず、必要なら `Ctrl-O r` で明示 resume する。

resume 不可の tab、選択されていない tab、および interrupted tab を持たない selection に対する `Ctrl-O r` は
daemon request を作らない。inventory refresh・reconnect・workspace open・planned restart も同様である。

planned な `daemon restart` は旧 generation の PTY を保持したまま control authority だけを移す別 failure mode である。
その間の tab は interrupted ではなく、owner generation の endpoint へ配送される live tab のままである
（[4. IPC の owner generation routing](04-ipc.md#owner-generation-routing)）。TUI client を owner routing に
載せるのは [#560](../.usagi/issues/560-feat-tui-client-ownerrouter-owner-generation-routing.md)、rollover 自体の起動は
[#559](../.usagi/issues/559-feat-daemon-standby-serve-owner-shard-seamless-rollover.md) の責務であり、
本 projection は crash / cold restart を旧 PTY の継続と偽らないことだけを保証する。

## feedback と終了

phase、operation / terminal error、disconnect、reconnect、resync は safe message と error ID だけを
TUI-local feedback として表示する。transport の内部 detail や secret は表示しない。orphan state では
terminal input を送らない。

`Ctrl-Q`（と live pane 上の `Ctrl-C`）は **exit prompt** を開く。この modal だけが workspace を出る唯一の
経路であり、`welcome`（Welcome へ戻る）／`quit`（TUI を閉じる）／`stay`（留まる）の 3 択を提示する。
どちらの答えでも daemon-owned の terminal や operation は停止しない。3 択の意味・キー・teardown は
[workspace の離脱と終了](#workspace-の離脱と終了)を正本とする。

modal は[共通 body-composition kit](#共通-body-composition-kit)の choice variant（`render_choice_over`）で
`[ welcome ] [ quit ] [ stay ]` を表示する。ボタンの幅と focus 表示は Yes/No の
[共通 confirmation component](#共通-confirmation-component)と同じ規則を共有するため、2 択と 3 択の見た目が
分岐しない。`Enter` は選択中のボタンを確定し、左右・Tab は focus を巡回する。modal を開いた時点の focus は
`quit` なので、`Ctrl-Q` + `Enter` は従来どおり TUI を閉じる。
