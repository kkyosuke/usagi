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
- [PR modal と browser effect](#pr-modal-と-browser-effect)
- [Sidebar mascot](#sidebar-mascot)
- [Closeup pane](#closeup-pane)
- [Closeup Agent の手動確認](#closeup-agent-の手動確認)
- [workspace open 時の pane 復元](#workspace-open-時の-pane-復元)
- [resume data compatibility](#resume-data-compatibility)
- [feedback と終了](#feedback-と終了)

## 画面と入力

Welcome は Open / Recent / New / Config の入口である。Open は登録済み workspace を名前の
大文字・小文字を区別しない alphabet 順に並べる。常時表示する Filter 欄は編集位置に cursor を
示し、入力した文字で即座に名前を絞り込み、↑↓ で絞り込み結果を選ぶ。各 workspace は名前と、session 数・未完了 issue 数・
最終更新の相対時刻を 2 行で表示する。Recent は同じ Workspace 画面を直接開く。New と Config は
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

作成が成功すると、その workspace を Open / Recent と同じ経路で開いて Home へ遷移する。作成が失敗すると、
入力中の draft を保ったまま notice を出して同画面に留まり、そのまま修正・再実行できる。作成の実行中は
入力を読まないため、`Enter` の連打で作成が二重に走ることはない。`Esc` は Welcome へ戻り、`Ctrl+C` は終了する。

Config は Theme・Modal mode・Agent model を scope（Global / Workspace）ごとに編集し、`↑↓` で行を、
`←→` で値を切り替える。未保存の値には `●` が付く。dirty な Save 行で `Enter` を押すと保存フローが始まり、
Save button 自体が **loading（`saving…`）** 表示に変わる。保存が成功すると同じ button が **`saved`** 表示へ変わり、
短い確認表示ののち、ユーザー操作なしで直前の Welcome へ自動的に戻る。保存が失敗した場合は自動で戻らず Config に
留まり、`Save failed: …` の notice を出す。draft は dirty のまま保たれるため、その場で確認・修正して再試行できる。
保存の実行中は入力を読まず、保存中の再押下（連打）は無視されるため、保存が二重に走ることはない。`Esc` は
Welcome へ戻り、`Ctrl+C` は終了する。

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

各 Effect は実 action を 1 回開始するか、安全な明示 error completion を 1 件返す。session create は成功時も
要求 token と作成された `SessionId` を持つ `OperationResult` を返す。失敗だけを返して成功を snapshot 更新に
暗黙化しない。terminal の `open` は同じ target の live terminal を再利用し、`new` は新規起動するため、
controller が正規化した引数を host まで保持する。notes と environment の保存は target の集合全体を永続化し、
decision / PR / browser / notification は daemon または platform adapter の結果を controller へ還流する。
従来 silent no-op だった操作も成功扱いせず、画面に安全な結果を返す。永続データの migration は発生しない。

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

`Root`（`⌂ root`）と `Session` はどちらも Agent / Terminal を作成できる scope である。Closeup の
`agent` / `terminal` action は active target から scope（root は `session_id` なし、session はその
`SessionId`）を導出し、pane は target ごとに独立して投影される。root の Agent / Terminal は daemon が
trusted repository root を cwd として起動し、live 出力・入力・resize・detach/reconnect は session pane と
同じ vocabulary で動く（設計根拠は [proposals/10-workspace-root-scope.md](proposals/10-workspace-root-scope.md)）。root 行は `x` / `X` による削除の対象外のままである。

daemon snapshot で selected または active の session が見つからなくなった場合、両方を同じ
workspace の root へ戻す。これにより、削除済み session を target にした古い local state を実行に
使わない。

Home の mode は Switch と Closeup である。Switch 中の右ペインは tab strip、content、footer を含めて dim
表示し、左 sidebar が操作対象であることを示す。Closeup では右ペインを active な明度へ戻す。Overview、Closeup action、PR、preview、text、notes、
environment、pending user decision、session 作成失敗 dialog は Home の背景を残す overlay として開き、最前面の overlay が入力を受け取る。diff は
Closeup pane の tab として開く。

Pending user decision は workspace ID で fence した daemon snapshot からだけ投影する。overlay は pending
一覧を表示し、選択すると title、prompt、option label/description、期限、freeform が許可された場合だけその
editor を表示する。Esc は editor から一覧へ戻り、一覧では overlay を閉じるだけで durable decision を変更しない。
submit は stable option ID または空でない許可済み freeform を送る。row は daemon の resolve confirmation まで
残り、resolve error・disconnect・resync 後も snapshot で再試行可能な pending state に収束する。modal が開いて
いる間は Home、Closeup、terminal の背景入力を dispatch しない。

新しい pending decision を resync で観測すると、Home header の右上に `🔔 N notice` を表示し、その直下の
banner に session identity（root は `workspace root`）と decision の title（summary）を表示する。ベルをクリックすると existing decision modal を
開き、未読表示を既読にする。modal が前面の場合はベル・banner を含む背景入力を受け取らない。未読は TUI-local の
stable decision ID 集合であり、同じ snapshot の replay、reconnect、resync は再び未読にしない。decision が
resolve/cancel/expire で pending snapshot から消えると未読も消える。

TUI は tick の resync で初回 snapshot 後に新規 ID を観測したときだけ desktop notification port を呼ぶ。通知本文は
session identity と title のみで、prompt・option・freeform answer は OS notification に送らない。port の配信失敗は safe に
無視し、TUI の banner / modal を継続する。合成ルートは macOS では `osascript`、Linux では `notify-send` を固定の
実行ファイルと引数ベクトルで spawn する。その他の OS、実行ファイル不在、headless notification service の失敗は
非対応として no-op である。

Home 背景の dim は各 ANSI span の reset 後にも維持し、行末で必ず reset する。overlay は dim 済みの背景へ
後から合成するため、modal 自身の style と可読性を優先する。

左 sidebar の marker は Home target 表示の正本である。Switch では selected cursor と current
target を別々に stable identity から照合し、同じ行なら cursor を優先する。Switch の cursor ではない
root / session / `+ new session` 行は v1 と同じ dim の非アクティブ色で描き、selected session の Accent は
保つ。Closeup では root / session を Accent で描き、current session だけを太字にする。`+ new session` は
色付けされるときは常に Success（緑）で、accent（青）へは決して落ちない。cursor が乗る Switch の選択時は
Success の太字、Closeup は Success の非太字で描き、太字は Switch の選択時だけに限る。Switch で cursor が
乗っていない `+ new session` は上記の非アクティブ dim に従う（この dim だけが Success 色を上書きする）。この Success 色は
full sidebar 行・rail の `+`・右ペイン preview 見出しで共有する単一の役割決定であり、生の ANSI 色ではなく
意味的 palette 役割で描くため、theme を retune しても追従し accent（青）へは落ちない。Closeup では cursor を
描かず、current marker だけを残す。session cursor はうさぎ `󰤇` と太字の名前、main と `+ new session`
の cursor は `>`、cursor ではない current target は緑の `▎` で示す。`+ new session` と pending
skeleton は current target にならない。名前・補足・marker は ANSI を閉じた表示幅で clip/pad するため、
CJK、Nerd Font glyph 未対応、極小幅でも後続行の style や列幅を壊さない。

Switch で cursor ではない session の補足行は、相対時刻・PR・Git summary の意味色を保ったまま dim にする。各
ANSI span の reset 後にも dim を再適用するため、current marker や Git の色 span が続いても相対時刻だけが明るく
戻らない。

Home controller の management input では、Switch の `Ctrl-A` は新規 session 作成フォームを開く。session 行を
選択中の `x` は `session remove`、`Shift`+`x`（`X`）は `session remove -f` を実行する。root と `+ new session`
行では削除しない。`Ctrl-Q` は workspace 終了確認を開く。Switch の `Ctrl-C` は何もしない。Closeup の live pane は `Ctrl-O` prefix 以外の
入力を所有するため、同じ control bytes は management transition に渡さない。Closeup の `Ctrl-O o` は
Switch へ戻り、Switch 中の `Ctrl-O` は単体では mode を変えない。Closeup action modal が前面にある間の `Esc` /
`Ctrl-C` は、`Ctrl-O o` と同じく modal を閉じて Switch へ戻る（live pane の有無に依らない）。overlay を開いて
いない Closeup の live pane 上の `Ctrl-C` が終了確認を開く契約はそのままで、他 overlay の `Ctrl-C` / `Esc`
契約も変えない。

左 sidebar は、root・実 session・`+ new session` の左クリックで cursor だけを移し、active target や mode を
変更しない。実 session は、同じ stable `SessionId` を 400ms 以内（境界を含む）にもう一度左クリックした場合だけ、
Enter と同じくその session を active target にして Closeup を開く。座標や表示順は同一性の判定に使わないため、
scroll や daemon snapshot によって同じセルの session が入れ替わっても Closeup を誤って開かない。root・
`+ new session`・divider・mascot・footer はダブルクリックの対象外であり、それらへの click は直前の session click と
結合しない。modal と inline 作成中は背景の sidebar click を受け取らず、その前後の click も結合しない。daemon
snapshot で session 一覧を置き換えた場合も、置換前後の click は同じ `SessionId` が残っていても結合しない。

Closeup の入力所有者は tab の有無で決まる。tab が無い Closeup は management input が所有し、action modal を
前面に出す。tab が 1 つ以上ある Closeup は `LiveInputClassifier` の `Ctrl-O` prefix（leader）が所有し、非
prefix の打鍵は live terminal への passthrough として扱う。TUI が予約するのは `Ctrl-O` prefix だけであり、
leader ではないすべてのキー入力は、修飾キーを含めて PTY へ送る。leader の follow-up は下表のアクションに解決し、それ以外は
消費する。tab 切替（`Ctrl-O` / `Ctrl-A` / `Ctrl-N` / `Ctrl-P`）は reducer が所有するが、scroll・tab close・copy は
reducer に持ち込まず shell と `TerminalSession` が所有する（scroll offset・選択・feedback は shell 側の状態）。

controller reducer path も同じ投影を使う。`LivePaneAvailability` が無い Closeup への遷移は action overlay を
自動で開き、pane が到着すると通常の tab surface へ戻る。adapter は prefix の next / previous 結果を
`CtrlN` / `CtrlP` として reducer に渡し、reducer は pane 所有者へ tab selection effect を要求するだけで、tab
identity は保持しない。

| prefix | アクション | 効果 |
|---|---|---|
| `Ctrl-O` `Ctrl-O` | Switch | Closeup から Switch へ戻る |
| `Ctrl-O` `Ctrl-A` | OpenCloseupModal | Switch では選択 target の Closeup action を開く。Closeup では tab があっても action modal を前面に出す |
| `Ctrl-O` `Ctrl-N` | NextTab | 次の tab を選ぶ |
| `Ctrl-O` `Ctrl-P` | PreviousTab | 前の tab を選ぶ |
| `Ctrl-O` `x` / `Ctrl-O` `Ctrl-X` | CloseTab | 選択中の tab を閉じる（live なら subscription を detach、pending なら起動待ちを取消） |
| `Ctrl-O` `u` / `↑` | ScrollUp | 右ペインの scrollback を 1 行古い方向へ |
| `Ctrl-O` `d` / `↓` | ScrollDown | 右ペインの scrollback を 1 行 live bottom 方向へ |

follow-up の `x` / `Ctrl-X` / `u` / `d` / `↑` / `↓` は leader が生きている間だけ予約し、leader 無しの単体キーは PTY へ送る。
leader は 1 秒で失効し、未知の follow-up は 1 打鍵だけ握って捨てる。leader 待機中の次の入力は prefix の
follow-up として扱う。

## Session sidebar rows

Home sidebar は `main → divider → session* → + new session` の順序と target identity を保つ。main と作成 action は
1 行、各 session は固定 2 行で描画する。`Sessions` 見出しは表示せず、session が 0 件でも main の直後に divider を置く。作成中の skeleton は `+ new session` の直前に置く。session の 1 行目は cursor / active marker、表示名、常に幅を
予約する note icon を表示する。note icon は既存の text overlay を開く入力を増やさず、内容の有無だけを示す。

2 行目は daemon snapshot の `last_active`、または旧 record の `created_at` を基準に、`now`、`12m ago`、`3h ago`
のような相対時刻で表示し、dismissed でない PR があれば先頭の PR 番号と残り件数を続ける。Git の検査が完了した session は、remote の既定 branch（`origin/HEAD`）を優先した base との差分として `↑ahead ↓behind + added - removed` を続ける。base branch 名は表示しない。追加数は緑、削除数は赤で描く。相対時刻・commit 差分・追加数・削除数は、表示中の全 session で共有する固定幅の列に配置する。検査は sidebar の描画とは別スレッドで行い、完了後は 1 秒以上あけて現在の session 集合を再検査する。未完了・取得不能・意味を持たない base branch 自身の状態は表示しない。PR title の解決はこの行の前提にしない。snapshot に無い
session は selected / active を main に縮退させ、空一覧でも main と作成 action は残る。

Switch で `+ new session` を選び Enter（または `t`）を押すと、その行が `+ new: <name>` の
inline 入力欄へ置き換わる。置き換わった入力欄でも `+ new` affordance は静的な `+ new session` と同じ
Success（緑）で描き、入力中の名前は accent で描くため、静的行から入力欄へ移っても affordance の緑が途切れない。
入力欄はその行が入力を所有しているため、選択を表す `>` chevron は描かず、空のマーカー列で affordance を静的
ラベルと揃える。名前を入力して Enter を押すと通常の `session create <name>` と同じ daemon
request を非同期に開始し、完了まで行の直前に session と同じ 2 行の skeleton を表示する。skeleton の activity glyph と session 名は Accent（青）で同じ
左から右へ流れる低速の wave で描き、静的な点滅にはしない。daemon が同一 `OperationId` と revision を持つ `session.created`
完了 hook を返したときだけ、skeleton をその response 内の snapshot row に置き換えて loading を終了する。IME に依存しない `Ctrl-A` も
同じ inline 入力を開く。`Ctrl-A` は選択カーソルも `+ new session` 行へ移動する。Esc は入力を取り消す。作成は名前だけを受け取り、profile / model は指定せず daemon の workspace default policy に委ねる（中央 modal ではなく行内の name-only 入力である）。入力中は英数字・`-`・`_` 以外、64 文字超過、または表示中 session と重複する名前を caret 行の下に error として表示し、空の名前は Enter 時に error を表示する。error は caret 行と同じ 1 行に詰めて末尾を切り捨てるのではなく、sidebar 幅（`unicode-width` 準拠の表示桁数）に合わせて caret 行の**下へ折り返して**表示するため、CJK を含む長い安全文でも切れずに読める。折り返した行数は `+ new session` 行の高さ計上（viewport の scroll 起点と footer）と一致させ、error が伸びてもレイアウトがずれない。これらは local validation で daemon へ送る前に弾き、入力（draft）は失わないので、error を直して再送できる。local validation の error（入力に付随）と、daemon が受付後に作成を拒否したときの表示は別物として扱う。前者は入力欄の直下に出し、後者は下記の作成失敗 dialog で安全な message だけを提示する。
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
sidebar と session 選択 modal は daemon snapshot の `available` session だけを表示する。作成に失敗した
reservation は durable state に残っていても選択対象にしない。

`env` は現在 active な target（workspace root か session）の environment editor を開き、その target の
永続化済み環境変数を読み込んで表示する。正本は notes・todos・decisions と同じ repository の `state.json`
（workspace state store）で、root と各 session が独立した `name → value` の集合を持つ。editor では変数の
追加・編集・削除ができ、保存すると集合全体が `state.json` に書き戻される（保存は差分ではなく全置換で、
消した変数は取り除かれる）。読み込み中と保存中はその状態を表示し、保存中の再保存は受け付けない
（二重送信の防止）。読み込みや保存が失敗した場合は editor に留まり、入力を失わずに安全な error を表示して
再試行できる。`env` は引数を取らないため、余分な引数を与えた場合は editor を開かず安全な notice で拒否する。

## PR modal と browser effect

`p` の PR modal は、focused `SessionId` の daemon PR snapshot だけを表示する。snapshot の revision が
同じか古い値、または別 session の値は捨てる。`pr.updated` は再取得の hint であり、event payload を
表示の正本にしない。snapshot が取れない場合は安全な unavailable 表示に留まり、legacy workspace state
や TUI scanner を production の fallback にしない。Open、Closed、Merged、Dismissed と title を表示し、
dismissed を新規検出として通知しない。

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
| Home の Quit（detach 確認） | Yes/No ボタン（既定） | `Enter/y: yes   Esc/n: no   ←→/Tab: choose` |
| open の Unregister workspace | Yes/No ボタン（既定） | 同上 |
| open の registry cleanup | compact（ボタンなし） | `y: remove   n/Esc: cancel` |

ボタン付き variant の Yes/No 選択状態は `ConfirmationModal` が持ち、compact variant は選択状態を
持たない（state 引数を読まない）。open の cleanup は list 本文に手組みしていた `y/n` prompt を廃し、
unregister と同じ overlay 経路で合成する。

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
pending tab を安全な feedback に置き換える。`←` / `→`（または `h` / `l`）と `Ctrl-O Ctrl-N` / `Ctrl-O Ctrl-P` は tab を巡回し、`Ctrl-O x` / `Ctrl-O Ctrl-X` は
選択 tab を閉じる。close 後は次の tab（末尾なら直前）を stable identity で選択し、最後の tab を閉じたときだけ
target selection と Closeup action の空状態へ戻る。close は client-side selection を外すだけであり、daemon-owned
terminal を停止しない。live tab の close は subscription を detach し、pending tab の close は起動待ちの launch を取り消す。

shell は毎フレーム全 attached terminal を poll し、daemon が exit を報告した terminal の tab を自動で閉じて
subscription を detach する。最後の live tab が exit すると `has_live_pane` が落ちて Closeup の action 空状態へ戻る。

Agent / terminal の launch は session create と同じく worker で実行し、daemon port は completion とともに UI へ返す。したがって
request を受け付けたフレームから completion まで pending chip は既存の共有 shimmer wave を表示し続け、入力は block されない。
completion が到着した後の次フレームでは、request 受付後に入力がなかった場合だけ同じ stable identity の live / document tab を選択する。

### live terminal の出力表示と入力

選択中の live terminal tab は、daemon が所有する PTY の出力を右ペインへ描画し、キー入力をその PTY へ
そのまま送る。TUI が使う同期 IPC client は push される stream event を受け取れないため、出力は **poll** で
取得する: launch 直後に一度 attach して保持済みの replay と output offset を受け取り、以降は redraw ごとに
`Resume { after_offset }` で offset 以降の出力だけを取得する。取得したバイト列は最小の VT screen（印字・
`CR` / `LF` / `BS` / `HT`・行折返し・カーソル移動・行/画面消去・scroll region を含む画面スクロール・SGR の色と属性・alternate screen buffer）へ流し込み、
その screen 行を右ペインへ clip して表示する。live の input cursor は現在セルを反転して表示する。output offset に gap があるとき、または daemon が
resync を要求したときは local に継ぎ足さず、daemon の atomic snapshot（再 attach）で置き換えて、その後の出力取得を継続する。

terminal pane の接続状態と footer feedback は `TerminalSession` の状態をそのまま投影する。

| 状態 | 入力 | poll / retry UX |
|---|---|---|
| `Live` | subscription と input sequence で送信 | output offset から継続取得する |
| `Reconnecting` | typed failure として拒否し、未配送を footer に表示 | 100ms から始まり 2s を上限とする指数 backoff 後、同じ `TerminalRef` を attach して snapshot resync する |
| `Disconnected` | typed failure として拒否 | stale target または明示的 detach の終端で、自動 retry しない |
| `Orphaned` | typed failure として拒否 | ownership unknown の終端で、自動 retry しない |
| `Exited` | typed failure として拒否 | 最終画面を保持し、自動 retry しない |

一時的な `unavailable` だけが `Reconnecting` へ遷移する。再 attach 成功時は backoff と input sequence を
reset し、新しい connection-owned subscription を使う。tab close / detach は予約済み retry を取り消す。
retry 中に replacement terminal を spawn せず、stale / orphaned / exited を一時切断として再試行しない。

primary screen から押し出された行は 10,000 行を上限とする local scrollback として保持し、right pane は live bottom を基準に
表示する。alternate screen のスクロールは現在の full-screen frame の一部であり、過去 frame を scrollback へ混在させない。ホイール上/下でそれぞれ古い出力方向／live bottom 方向へ 1 行移動する。新しい
replay で履歴が短くなった場合は offset を有効範囲へ正規化する。`↑` / `↓` は scrollback 操作に予約せず、PTY の
history navigation へそのまま送る。right pane の footer の直前には常に 1 行の空白を置く。

出力は mouse drag により選択でき、drag を離すと選択した ANSI を含まない表示テキストを OS clipboard にコピーする。drag 中も
drag を離した後も、選択範囲は右ペインに reverse-video で示し続ける。選択は右ペイン content 内の通常左クリック、次の drag が
新しい選択を始めるか、focus が別の terminal へ移るまで表示され続ける（release で即座に消えない）。この click は text selection
だけを解除し、sidebar の navigation / activation、modal の入力所有、PTY への入力を変えない。選択の可視化は選択した桁全体に及び、行末の空白 padding や
選択範囲に含まれる空行も反転する（agent が描く空白 padding 中心の画面でも選択が消えない）。キー入力は
コピーに使わず、`Ctrl-C` を含めて live terminal へそのまま送る。
clipboard adapter は macOS の `pbcopy`、Windows の `clip.exe`、
Wayland の `wl-copy`、X11 の `xclip` / `xsel` を現在の環境に応じて使う。利用可能な backend がない場合は copy を成功扱いにせず、
安全な feedback を表示する。

出力中の `http(s)` URL は左クリックで OS 既定ブラウザに開ける。URL が載るセルは下線で装飾し、クリック可能で
あることを示す。drag で非空の選択が成立した release は**コピー**、選択が生じない素のクリックだけを**リンクオープン**として扱い、
両者は排他である（選択中はリンクを開かない）。クリック位置のセルを保持中の行/列へ写し、行末で折り返した URL は 1 本に結合して
開く。URL 上でないセルのクリックは何も開かない no-op で、選択・scrollback offset を乱さない。検出・検証は純粋コアが担い（`http(s)`
スキームのみ許可し、制御文字・ESC・空白・非 ASCII を拒否する）、起動直前にも再検証してから argv で spawn するため、ANSI/端末制御
列がブラウザ引数へ渡らない。起動は [PR modal と browser effect](#pr-modal-と-browser-effect) と同じ browser effect（macOS `open` /
Linux `xdg-open` / Windows `cmd /C start "" <url>`）を使い、未対応 platform・起動失敗は TUI を乱さず safe feedback にする。
pointer の release は PTY へ入力として転送しない。

live terminal に focus がある間、leader ではないすべてのキー入力（文字・修飾キー・paste・raw bytes・Enter・Backspace・Tab・矢印など）は management ではなく
PTY へ送られる。矢印は対応する CSI 列、Enter は `CR` に符号化する。tab 巡回や Closeup/Switch の遷移は
`Ctrl-O` prefix（`Ctrl-O Ctrl-N` / `Ctrl-O Ctrl-P` / `Ctrl-O Ctrl-O`）だけが所有する。前面 modal や forced action modal がある間は
その modal が入力を所有する。入力は subscription と単調増加する input sequence で fence し、同じ打鍵を二重送信しない。
Live でない、subscription がない、または送信に失敗した入力は success と扱わず、未配送を safe feedback として footer に表示する。
terminal は起動時点と resize 後の右ペイン実幅・高さで geometry を要求するため、shell の right prompt も pane 内に収まる。geometry が変わると TUI は PTY と decoded local screen を resize する。過去の cursor 移動列は新しい幅で再生せず、過去行を含む既存セルを clip して行数を増やさない。daemon 不通・stale・orphan は安全な
feedback だけを表示し、local PTY を生成しない。

Closeup の `agent [profile]` は既存 session だけで実行できる。profile を省略すると daemon の
workspace policy を使い、指定時も product-neutral な profile ID だけを durable operation に渡す。
TUI は daemon の accepted response 後に Agent pending tab を置き、同じ operation の成功 final が返す
完全な `TerminalRef` にだけ attach する。daemon 不通、拒否、未知・古い completion では local spawn や
名前からの terminal 推測をしない。

daemon inventory、attach/resume、stream、resync は `pane_runtime` が結合する。output cursor に gap が
ある場合は local output を継ぎ足さず、daemon の atomic snapshot で置き換える。resize は geometry の
変化時に送り、失敗した場合は同じ geometry でも次フレームで再試行して、PTY と右ペインの VT screen を
同じ viewport に保つ。detach はこの client の
subscription を外すだけで、PTY を kill しない。daemon が exit を報告した terminal または Agent は、その
live tab と client subscription を直ちに外し、残る tab または Closeup の空状態へ戻る。

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
request retry、attach を行わない。failure は pending tab を除去し、daemon が安全と保証した文言を error modal
として表示するとともに `<data dir>/logs/error-YYYY-MM-DD.log` に記録する。確認して閉じると、tab-less
Closeup の action modal に戻る。

## Closeup Agent の手動確認

Agent profile を利用できる daemon を起動し、既存 session を選択して Closeup を開く。次の操作は実装済みの
runtime bridge を確認する手順である。profile の install 状態、認証内容、argv は画面に入力・表示しない。

| 操作 | 確認結果 |
| --- | --- |
| Action menu の Agent、または `agent codex` を確定する | 同じ session の `Agent` tab が出て、wave が daemon の pending operation を示す |
| matching final を daemon が replay する | pending が Agent tab に一度だけ置換され、選択中なら attach される |
| Agent が stdout を出力する | 選択中 Agent tab の pane に出力が表示される |
| 選択中 Agent tab で入力し、端末を resize する | 入力は一度だけ daemon に届き、geometry 変更時の resize は成功するまで再試行される |
| daemon を切断して再接続する | process を作り直さず、inventory で検証済みの選択 tab だけが attach/resync される |
| profile 未準備・daemon 不通・Agent exit を発生させる | pending/tab state は収束し、安全な error modal が表示され、日次 error log に記録される |

## workspace open 時の pane 復元

daemon は terminal / Agent runtime の権威 owner であり、TUI を閉じても runtime は daemon 内で継続する。
そのため workspace を開き直した（同じ client の再 open、または 2 つ目の client の open）とき、その
workspace の root scope と各 available session scope に属する **live**（現 daemon generation が所有し attach
可能）な Agent / Terminal を pane tab に復元する。

- **タイミング**: 初回 frame を paint した後に一度だけ、daemon の [`terminal inventory`](04-ipc.md#generic-terminal-request)
  を root と各 available session の scope について引く。初回 frame は inventory 応答を待たずに描画する。
- **投影**: `live` な各エントリを、その `kind` に応じて Agent tab または Terminal tab として、`session_id`（None=root）
  から導く target に投影し、完全な `TerminalRef` で fenced に attach する。target ごとの最初の復元 tab を pane 内で
  選択するため、その target の Closeup に入ると出力表示と通常入力配送を直ちに再開できる。復元は Home の selected / active
  target や Switch mode を変更しない。
- **source of truth は daemon の inventory** であり、TUI-local に pane 一覧を永続化しない。これにより別 client が
  起動した pane や、以前の TUI が保存しなかった runtime も一貫して復元できる。TUI-local resume state は表示・選択の
  復元候補に留める（下記 [resume data compatibility](#resume-data-compatibility)）。

誤復元・二重 tab を防ぐため、次を守る。

| 入力 | 判定 | 動作 |
|---|---|---|
| `live: false`（死んだ process / exited / orphan / identity_unknown） | attach 不可 | tab を作らない。PTY master 復元不能は session 単位の interrupted 契約に委ねる |
| stale / recreated session | inventory 問い合わせ scope が現 lifecycle snapshot の available session に限られる | 旧 session の runtime は列挙されず復元されない |
| scope mismatch（別 workspace / worktree / session） | daemon が scope 完全一致で filter | 列挙されない |
| daemon generation 不一致 | `TerminalRef::fences` 不一致 | attach しない |
| duplicate entry | `fences` で既存復元 tab と一致 | dedup、二重 tab を作らない |
| daemon 不通 / inventory 失敗 | 取得失敗 | 何も復元せず local PTY も作らない |

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

`q` は確認後に TUI だけを閉じ、daemon-owned terminal は継続する。`Ctrl-Q` も同様に detach 確認 modal を
開き、確認するとこの TUI client だけが detach する（daemon-owned の terminal や operation は停止しない）。
確認 modal は[共通 confirmation component](#共通-confirmation-component)（`render_confirmation_over`）の
Yes/No variant で `[ yes ] [ no ]` を表示し、`Enter`（選択中のボタンを確定）、左右・Tab（Yes/No 選択の切替）、
`y`（detach）、`n` / Esc（留まる）で操作できる。
