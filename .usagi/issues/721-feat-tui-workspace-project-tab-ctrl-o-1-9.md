---
number: 721
title: feat(tui): 複数 workspace を project tab で開き Ctrl-O + / 1..9 で操作する
status: done
priority: medium
labels: [feat, v2, tui, ux, navigation]
dependson: []
related: [77, 224, 239, 506, 549, 556]
created_at: 2026-08-24T15:00:00+00:00
updated_at: 2026-08-25T23:13:26.406377+00:00
---

## 目的

1 つの TUI process 内で複数 workspace を project tab として開き、上部の tab bar とキーボードで即座に切り替えられるようにする。

本 issue の「一画面」は複数 workspace の session を同じ sidebar に混ぜる aggregate view ではなく、複数 project tab を常時見せ、
選択した 1 workspace の Home を全面表示する意味とする。

```text
┌ 1 usagi ┐  2 api    3 web    + Open
└─────────────────────────────────────────────────────────────────
│ active workspace の既存 Home（sidebar / Closeup / Director）
```

## 現状と根拠

- screen graph は Welcome / Open / New から 1 workspace の `drive_workspace_controller` へ入り、離脱時にその workspace の
  `ControllerBackendComposition` と背景 lane をすべて drop する。別 workspace は同じ process で順番に開けるが、複数 workspace を
  開いた状態として保持・表示できない（#556）。
- daemon は client が選んだ workspace を adopt し、複数 workspace を tenant として同時に serve できる（#549 以降の現行契約）。
- Open view には `Tab` で Unite mode、`Space` で複数選択する状態が既にあるが、`OpenStep::Choose(PathBuf)` は選択集合の先頭 1 件しか
  開かない（#239 の残存 gap）。
- `Recent::Unite` / `UniteOverview` と Welcome の描画も存在するが、core の `recent()` は single workspace だけを生成し、Unite recent を
  永続化・再入場する経路がない。
- live pane の `Ctrl-O` leader は 1 秒の prefix state machine を持ち、数字と `+` は現在未割当である（#224）。

## 確定する UX

### project tab bar

- Home の最上段に project tab bar を常時 1 行表示する。1 workspace のときも `+ Open` を見せ、追加導線を隠さない。
- tab は deck 内の安定した順序で `1` から採番する。active tab は Accent + bold、inactive tab は dim で描く。
- 端末幅に収まらない場合は active tab が必ず見えるよう横方向に windowing し、隠れた件数を `… +N` で示す。名前の clip と幅計算は
  `unicode-width` を使う。
- tab の identity は表示名や index ではなく canonical workspace path（daemon attach 後は `WorkspaceId`）で持つ。番号はその frame の
  操作 shortcut であり durable identity にしない。
- tab click は切替、`+ Open` click は追加 overlay を開く。hit test は描画時に確定した identity を返し、並び替え後の index を再解釈しない。

### キー操作

| 入力 | 動作 |
|---|---|
| `Ctrl-O` → `+` | Add workspace overlay を開く |
| `Ctrl-O` → `1` … `9` | deck の 1〜9 番目を active にする |
| `Ctrl-O` → `0` | 全 tab を一覧する project switcher を開く。10件目以降もここから選ぶ |
| tab click | その workspace を active にする |
| `+ Open` click | Add workspace overlay を開く |

直接の `Ctrl+1` … `Ctrl+9` と `Ctrl++` は標準 binding にしない。legacy terminal encoding では Control と数字・記号の組を一意に
報告できず、端末によって plain digit、`Ctrl+=`、`Ctrl+Shift+=` などへ揺れるためである。既存の portable な `Ctrl-O` leader に続けることで、
live PTY 内でも同じ入力を確実に予約する。

数字と `+` / `0` は、Switch、Closeup、live terminal、Director のどこからでも deck action が先に解決する。既存の `Ctrl-O` follow-up
（tab 操作、scroll、Director 等）は変えない。overlay の編集 input 中も deck shortcut は予約するが、未保存 draft がある場合の workspace
切替は拒否し、draft を保持して安全な notice を表示する。

### Add workspace overlay

- `Ctrl-O +` は現在の Home を背面に保持したまま、登録済み workspace の filter/list を前面に出す。
- 現在 deck に含まれる workspace は checked + disabled として表示する。未追加 workspace は `Space` で複数選択し、`Enter` で deck 末尾へ
  registry 順に追加する。
- `Esc` は選択を破棄して元の workspace へ戻る。active composition と pane selection を壊さない。
- 0 件の `Enter` は副作用なしで留まる。重複 canonical path は追加しない。
- 選択集合の attach は UI 上 all-or-nothing とする。1 件でも workspace fence / settings / snapshot load に失敗したら deck へ 1 件も追加せず、
  選択を保ったまま失敗 workspace と安全な理由を notice に出す。
- 成功時は追加した最初の workspace を active にする。

### project switcher と close

- `Ctrl-O 0` は全 tab を順番どおり表示し、↑↓ / 数字 / Enter で切り替える。
- switcher の `x` は選択 project tab を deck から閉じるだけで、workspace の unregister、session 削除、daemon terminal 終了は行わない。
- active tab を閉じた場合は右隣、なければ左隣を active にする。最後の 1 件を閉じた場合は既存の安全な detach を行って Welcome へ戻る。
- 内側の pane tab close である `Ctrl-O x` の意味は変えない。project tab の close は switcher 前面だけが `x` を所有する。

## runtime / ownership 設計

### WorkspaceDeck

process-level screen graph に次を所有する `WorkspaceDeck` reducer を追加する。

```text
WorkspaceDeck
├── slots: Vec<WorkspaceSlot>  // canonical path / WorkspaceId / label / order
├── active: WorkspaceId
├── overlay: None | Add | Switcher
└── notice: Option<String>
```

`WorkspaceDeck` は membership、順序、active identity、overlay、notice だけを所有する。session / pane / modal / Agent state は既存の
workspace controller の責務に残し、workspace 間で共有しない。

### active composition は 1 件だけ

- TUI が daemon 接続と `ControllerBackendComposition` を保持するのは active workspace 1 件だけとする。inactive slot ごとに session refresh、
  metrics、restore、terminal stream の resident connection を常駐させない。
- 切替前に target の canonical root を daemon へ申告して fresh `WorkspaceSnapshot` と settings を同期的に準備する。成功結果の target identity を
  確認してから current composition を drop し、target composition を生成する。
- prepare に失敗した場合は current composition と active identity を不変に保ち、notice を表示する。prepare は同じ frame loop 内の同期処理なので
  late / duplicate completion 自体を生成しない。
- current composition の drop は #556 と同じ detach 契約を使う。client subscription と背景 lane は解放するが、daemon-owned terminal / Agent /
  operation は停止しない。
- target entry は daemon inventory と durable Agent tab intent（#506）から pane を復元する。これにより inactive 中も処理は継続し、再選択時に
  同じ runtime identity へ attach する。
- settings の workspace binding は activation commit と同時に target へ切り替える。prepare failure や stale completion が current workspace の
  binding を上書きしない。
- MVP では cursor、scroll offset、開いた一時 modal など TUI-local ephemeral state は workspace 切替後に初期化してよい。ただし未保存 editor /
  inline draft は切替を拒否して失わない。durable tab intent、daemon terminal、Agent、session state は保持する。

この active-only 方針により、deck の件数に比例した daemon connection / worker / frame polling を避ける。複数 workspace を同じ sidebar に混ぜる
v1 Unite aggregate（#77）や複数 workspace の live pane を同時描画する split view は対象外とする。

## input 境界

- live pane では `LiveInputClassifier::prefix_action` に deck action（digit / `+` / `0`）を追加し、PTY bytes より先に process shell へ返す。
- management surface でも同じ 1 秒の `Ctrl-O` window で deck action を解決する。現在の単体 `Ctrl-O` management transition は即時に維持し、
  後続が deck action でない場合はその後続 input を従来どおり 1 回だけ処理する。
- deck action は active workspace の `AppState` / pane reducer に入れず、process-level `WorkspaceDeck` が消費する。
- semantic key と raw control byte の双方、Press / Repeat / Release、timeout 境界を pure classifier test で固定する。

## persistence

- 開いた deck の ordered canonical paths と更新時刻を user-data scope の versioned/atomic store に保存し、registry の workspace 値と read-time join
  して `Recent::Unite` を組み立てる。workspace repository 配下には保存しない。
- 同じ ordered set は 1 件に正規化して touch する。single workspace recent と Unite recent を `updated_at` で同じ一覧に並べる。
- unregister / missing path は read-time に member から除外する。0 件へ退化した Unite は表示せず、1 件へ退化しても persisted record は勝手に
  single へ書き換えない。
- Welcome の Unite card を選ぶと deck を同じ順序で再度開く。現在 no-op の `Recent::Unite` selection を実経路へ接続する。
- store の future version / parse error は他の registry entry を破壊せず、安全な notice と single-workspace recent への縮退にする。

## 実装フェーズ

1. **deck / input foundation**: pure `WorkspaceDeck` reducer、project bar projection、`Ctrl-O + / 0 / 1..9` classifier、描画・hit test。
2. **runtime switch**: monolithic workspace loop を active composition の prepare / commit / drop が可能な step 境界へ分け、1 active composition の切替と
   失敗 fence を実装。
3. **add / switcher**: 既存 Open filter・multi-select を reusable overlay にし、batch add、tab close、10件以上の選択を実装。
4. **Unite persistence / re-entry**: versioned store、`recent()` join、Welcome Unite card の再入場を実装。
5. **docs / E2E**: `document/03-tui.md`、必要な architecture/data ownership、README の操作表を実装済み仕様へ更新し、shipping PTY E2E を追加。

各フェーズは intermediate main を安全に保ち、未接続の UI や現在の build で動かない仕様を `document/` に先行記載しない。

## 受入条件

- [x] 2 件以上の workspace を 1 TUI の project tab として開き、上部に同時表示できる。
- [x] `Ctrl-O 1` / `Ctrl-O 2` と tab click で workspace を切り替え、plain `1` / `2` は live PTY へ従来どおり 1 回届く。
- [x] `Ctrl-O +` と `+ Open` click から 1 件または複数 workspace を既存 deck へ追加できる。
- [x] 10 件以上でも `Ctrl-O 0` の switcher から全件へ到達でき、active tab は狭い端末でも bar 内に見える。
- [x] 切替中の target attach 失敗は current workspace を閉じず、同期 prepare は stale / duplicate completion を生成しない。
- [x] inactive workspace の daemon-owned Agent / terminal は継続し、再選択時に新規 spawn せず exact runtime へ再 attach する。
- [x] 同時に resident な workspace composition は常に 1 件で、切替後に旧 workspace の port / worker / subscription が残らない。
- [x] Add cancel / attach failure / dirty editor の切替拒否で入力 draft と deck membership を失わない。
- [x] project tab close は detach のみで、workspace unregister、session 削除、terminal 終了を起こさない。
- [x] deck を終了・再起動して Welcome の Unite recent を選ぶと、同じ順序の tab set が復元される。
- [x] direct `Ctrl+digit` / `Ctrl++` に依存せず、対応端末・非対応端末で標準操作が一致する。
- [ ] coverage 100% を維持する。

## 必須テスト

- pure reducer: add dedupe/order、activate identity、close fallback、last close、10件以上、同期 prepare の failure fence。
- input classifier: live / management / Director / overlay、digit / plus / zero、timeout、unknown follow-up、plain digit passthrough、Press / Repeat / Release。
- render: 1件・複数・狭幅・CJK・overflow、active visibility、identity-bearing mouse hit。
- screen graph fake: 2 workspace の prepare→drop→activate 順、prepare failure 時の current 保持、old ports が next composition 作成前に全 drop されること。
- daemon fake: inactive 中に Agent output / session snapshot が進み、再選択時に same `TerminalRef` へ attach して spawn count が増えないこと。
- add overlay: multi-select all-or-nothing、cancel、重複、部分 failure。同期 prepare のため late completion は発生しない。
- persistence: version round-trip、ordered-set dedupe、missing member、future/corrupt version の安全な縮退、Unite recent 再入場。
- shipping PTY E2E: workspace A で Agent を起動 → `Ctrl-O +` で B を追加 → `Ctrl-O 2` / `Ctrl-O 1` で往復 → A の同じ Agent が
  継続し、plain digit は Agent に届く。

## 対象外

- 複数 workspace の session を同じ sidebar に混ぜる aggregate view（#77）。
- 複数 workspace の live pane を同時に左右分割表示すること。
- inactive workspace ごとの常駐 background observation / unread badge。
- direct `Ctrl+digit` / `Ctrl++` の best-effort alias。
- project tab close による unregister / session remove / terminal exit。
