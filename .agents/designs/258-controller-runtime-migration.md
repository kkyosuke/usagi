# 設計: 実端末 Workspace runtime を controller 経路へ移行する

**対象 issue**: #258「fix(tui): Home の root-first row contract を runtime まで一元化する」
**目的**: 実端末の Workspace runtime（`presentation::drive_workspace_*` + 旧 `Workspace` view）を、controller の `AppState` / `HomeProjection` / `render_home` 経路に一本化し、Home 画面の state・入力・描画の二重定義を解消する。

---

## 1. 現状整理

### 1.1 二系統の並存

v2 TUI には現在、同じ Workspace 画面を定義する 2 つの系統がある。

```
【実端末で動いている系統（旧経路）】
main → runtime::tui::launch → run_in_terminal（raw mode / alt screen）
     → presentation::run_with_settings_inner（screen graph）
     → drive_workspace_with_agent_port_and_selection_mode   … フレームループ
          state : WorkspaceUi { workspace: 旧 Workspace view, modal, terminals, … }
          input : term.read_key() → step_workspace → step_switch / step_closeup
          render: render_workspace → workspace::render_with_skeleton_frame

【テストでのみ検証されている系統（controller 経路）】
AppState（usecase/application/controller.rs）
  ├ update(state, AppEvent) -> Vec<Effect>     … 純粋 reducer
  ├ HomeProjection::from_state(...)            … 描画用投影
  └ render_home(h, w, &projection)             … ANSI 行 Vec<String>
呼び出し元: parity_suite.rs / unit tests / AgentRuntime（LaunchAgent のみ実配線）
```

controller 経路は #256 / #267 / #269 / #279 / #293 / #295 / #305 で reducer・投影・入力契約が整備済みだが、**実端末のフレームループには接続されていない**。旧 `Workspace` view が実 runtime の source of truth のまま残っている。

### 1.2 issue #258 調査結果からの現状差分

issue 本文の調査結果のうち「旧 `Workspace` は sessions → root を保持する」は現行コードでは解消済みで、旧 view も既に root-first（`selected == 0` が root）である。**残っている本質的な課題は行順ではなく、次の 2 点**。

1. 旧 view が `selected: usize` の index 契約（`0` = root、`len+1` = `+ new session`）で navigation・render・hit-test を推測しており、controller の stable な `Selection` / `Target` identity と二重定義になっている。
2. 実端末の入力・描画・イベント dispatch が controller（`update` / `HomeProjection` / `render_home`）を経由していない。

### 1.3 controller 経路の完成度（ギャップ一覧）

旧 `Workspace` view / `WorkspaceUi` が担う機能と controller 側の対応状況。

| 機能 | 旧経路の所有 | controller 側 |
|---|---|---|
| Home 行の選択・wrap・活性化 | `selected: usize` + `step_switch` | **あり**: `Selection` / `Target`, `move_selection`, `activate_selected` |
| Switch / Closeup 遷移 | `Mode` | **あり**: `Route::Home(HomeMode)` |
| pane tab strip / tab 移動 | `panes`（session 名キー） | **あり**: `PaneRuntime` 所有 + `HomeProjection::with_pane`, `Effect::SelectTab`（#256 / #267） |
| Ctrl-O prefix / Closeup 入力契約 | `apply_live_action` ほか | **あり**: `AppKey::CtrlO/CtrlA/CtrlN/CtrlP` + `Overlay::Closeup`（#269 / #279 / #305） |
| session 作成（inline form / pending skeleton） | `create_input` / `pending_session` | **あり**: `Overlay::CreateSession` + `CreateSessionForm` + `PendingOperation`（runtime 接続は #287 が担当） |
| notes / environment / remove / quit 確認 | `WorkspaceModal` 群 | **あり**: `Overlay::Notes/Environment/QuitConfirmation` + 各 `Effect` |
| live terminal 行の描画投影 | `terminal_view` / `terminal_scroll` | **なし（ギャップ G1）** |
| daemon metrics（mascot sidecar） | `metrics` | **なし（ギャップ G2）** |
| git 差分列（sidebar） | `git_diffs` | **なし（ギャップ G3）** |
| PR modal / preview overlay | `PrModal` / preview | **なし（ギャップ G4）** |
| pointer（sidebar クリック / terminal drag） | `sidebar_row_at` / `handle_terminal_pointer` | **なし（ギャップ G5）** |
| Effect の本番実行 | （旧経路は直接 port 呼び出し） | **`LaunchAgent` のみ**（`AgentRuntimeHost`）。他 13 variant の実行 adapter が無い（**ギャップ G6**） |

---

## 2. ゴール / 非ゴール

### ゴール（issue #258 の完了条件に対応）

- 実端末の Home 描画・入力経路が `AppState` → `update` → `HomeProjection` → `render_home` を経由し、旧 `Workspace` の row state が実 runtime の source of truth として残らない。
- 行契約は常に `root → sessions (snapshot order) → + new session`。初期 selected / active は root。`↑/↓/j/k` の wrap、Enter / `t`、selected（`>`）と active（`*`）の独立表示が controller の 1 実装で担保される。
- viewport・empty sessions・tiny geometry の不変条件を controller 側テストに集約する。
- #295 / #305 で配線済みの live pane / terminal 挙動を退行させない。
- #287（`+ new session` / Ctrl-A の create entry 接続）が乗れる seam を残す。

### 非ゴール

- 右ペイン tab の可視性・layout・入力 semantics の変更（別 triage のスコープ。`render_home` の既存 tab strip 実装をそのまま使う）。
- session lifecycle / daemon snapshot transport / pane 機能の追加。
- sidebar の visual redesign。
- G4（PR modal / preview）と G5（pointer 語彙）の controller への完全移設 — 本設計では暫定 seam に留め、後続 issue に切り出す（§8）。

---

## 3. 方針の選択

| 案 | 内容 | 評価 |
|---|---|---|
| A. 一括置換 | `WorkspaceUi` / 旧 `Workspace` / `step_*` / `render_with_skeleton_frame` を 1 PR で削除し controller 経路に置換 | ギャップ G1〜G6 を同時に埋める必要があり、mod.rs 70 + workspace.rs 54 のテスト移植も同時発生。レビュー不能な巨大 PR になる |
| B. strangler（採用） | `WorkspaceUi` を runtime shell として残し、内部の Home state を `AppState` に、描画を `render_home` に差し替える。ギャップは「投影の拡張」「Effect executor の新設」「shell 暫定保持」に分類して段階的に埋める | 各段階が独立に検証可能。done issue（#295 / #305 等）の退行リスクを parity テストで抑えられる |
| C. 旧 view を controller の facade 化 | 旧 `Workspace` のメソッドを内部で `AppState` に委譲 | 二重 API が恒久化し、issue の「旧 contract を残さない」に反する |

**案 B を採用する。** 移行の単位は「投影（render）→ 実行（effect）→ 接続（loop）→ 掃除（削除）」の 4 段階（§5）。

---

## 4. 詳細設計

### 4.1 state の行き先マッピング

旧 `Workspace` view の各フィールドの移行先。**原則: 行選択・モード・overlay は `AppState`、描画素材は `HomeProjection` の入力、live IO のハンドルは runtime shell（`WorkspaceUi` 後継）が所有する。**

| 旧フィールド | 行き先 | 備考 |
|---|---|---|
| `mode: Mode` | `AppState.route`（`HomeMode`） | 削除 |
| `selected: usize` | `AppState.selected: Selection` | **usize index 契約の廃止**。`rows()` が唯一の行定義 |
| `record` / `state.sessions` / `session_ids` | `AppState.sessions: Vec<SessionId>` + 投影入力 `snapshot_sessions: &[ProjectedSession]` | snapshot は shell がキャッシュし ID 結合は `from_state` 既存実装 |
| `panes` / `pane_documents` | `PaneRuntime`（既存）+ `HomeProjection::with_pane` | 旧の session 名キー投影を削除、stable `Target` キーに一本化 |
| `pending_session` | `AppState.pending: Vec<PendingOperation>` | 削除 |
| `create_input` / `create_error` | `AppState.create_session: CreateSessionForm` | inline 編集は overlay form に置換。#287 の接続 seam |
| `metrics` | **新規** `HomeProjection::with_metrics(DaemonMetrics)` | G2。state ではなく投影入力(描画にしか使わない） |
| `git_diffs` | **新規** `HomeProjection::with_git_diffs(&BTreeMap<SessionId, GitDiff>)` | G3。同上 |
| `terminal_view` / `terminal_scroll` / `terminal_feedback` | **新規** `HomeProjection::with_terminal_view(TerminalViewProjection)` | G1。行データは shell が `TerminalSession::poll` で取得。scroll offset は shell 所有（reducer に持ち込まない） |
| `pane_owner`（fencing） | `AppState` の既存 fence 検証（`RuntimePhase` 反映時） | 削除 |

`AppState` は純粋 reducer のまま保つ。**PTY 行・metrics・git diff のような「毎フレーム外から来る描画素材」は `AppState` に入れず、`HomeProjection` のビルダー入力にする**（`with_pane` / `with_mascot_speech` と同じ既存パターンの踏襲）。

### 4.2 入力: `Key` → `AppEvent` アダプタ

`Terminal` port（`CrosstermTerminal`）は今後も legacy `Key` 語彙を返す。presentation に変換関数を新設する。

```rust
/// presentation/mod.rs（新設）
fn app_event_from_key(key: Key) -> Option<AppEvent>
```

- `Key::Live(action)` → 対応する `AppKey`（`CtrlO` / `CtrlA` / `CtrlN` / `CtrlP` / `OpenCloseupOverlay` …）。Ctrl-O prefix の解決は従来どおり `LiveInputClassifier`（実端末 adapter 内、SSoT）が行い、reducer には確定済み `AppKey` だけが届く。
- 通常キー → `classify_management_input`（controller 既存）を再利用し `AppEvent::Key(AppKey)` へ。
- resize → `AppEvent::Resize`、tick / backend wakeup → `AppEvent::Tick` / `AppEvent::Backend`。
- `Key::Passthrough(bytes)` は reducer に入れない。shell が「controller が Closeup かつ live pane focus」のとき（`state.route()` と `has_live_pane` で判定）だけ `PaneRuntime` へフォワードする。現行 `forward_terminal_input` のゲート条件を controller state 参照に置き換える。

**pointer（G5、暫定 seam）**: sidebar クリックは shell が hit-test して `AppEvent` に翻訳する。hit-test は旧 `sidebar_row_at` の index 演算をやめ、`HomeProjection` に `row_at(y) -> Option<Selection>` を新設して projection の viewport（`home_viewport_start`）と同じ計算を共有する。クリック行への選択移動は、reducer に `AppKey::SelectRow(Selection)`（新設、pointer 専用）を追加して 1 event で表現する。terminal pane 内の drag / copy は Home 行契約と無関係なので shell + `TerminalSession` に残す。

### 4.3 Effect 実行: `DaemonBackend`（G6）

`controller::BackendPort` の本番実装を usecase/application に新設し、既存 daemon port 群を束ねる。実 IO は従来どおり合成ルート（`src/runtime/tui.rs`）が注入する。

```rust
/// usecase/application/daemon_backend.rs（新設）
pub struct DaemonBackend {
    session_commands: Box<dyn SessionCommandPort>,   // worker thread + mpsc（現行 begin_session_create の方式を移設）
    agent_host: AgentRuntimeHost<...>,               // LaunchAgent / OpenTerminal / SelectTab / resize（既存 #295 資産）
    overlay_data: Box<dyn OverlayDataPort>,          // notes / environment / preview 素材
    notes_store: ...,                                // Load/SaveNotes, Load/SaveEnvironment
    completions: mpsc::Receiver<AppEvent>,           // 非同期完了 → OperationResult / BackendEvent
}
```

| `Effect` variant | 実行先 |
|---|---|
| `CreateSession` / `RefreshSessions` / `RemoveSession` | `SessionCommandPort`（worker + mpsc、完了は `OperationResult` / `BackendEvent::Sessions` で還流） |
| `LaunchAgent` | 既存 `AgentRuntimeHost::dispatch`（変更なし） |
| `OpenTerminal` | `AgentLaunchAdapter` 経由で `PaneRuntime`（#303 / #295 の配線を利用） |
| `SelectTab` | `PaneRuntime` の tab 選択 |
| `WorkspaceCommand` | 現行 Overview コマンド実行の移設 |
| `LoadNotes` / `SaveNotes` / `LoadEnvironment` / `SaveEnvironment` | notes / environment store port（完了は `BackendEvent::NotesLoaded` 等） |
| `Detach` | shell へ `Exit` を返しループ脱出 |
| `AttachWorkspace` / `CloneProject` / `RegisterWorkspace` | Workspace 画面では未使用（screen graph 側）。`debug_assert` + no-op で受け、screen graph 移行時に接続 |

非同期完了はすべて `AppEvent`（`OperationResult` / `Backend(BackendEvent)`）として mpsc に載せ、フレーム先頭で drain して `update()` に流す。**「effect を出す → 実行する → 結果 event が reducer に戻る」の単方向ループ**に統一し、旧経路の「view メソッドを直接叩いて state を書き換える」パターン（`apply_session_projection` 等）を廃止する。

### 4.4 描画: `HomeProjection` 拡張 + `render_home`

- `HomeProjection` に `with_metrics` / `with_git_diffs` / `with_terminal_view` / feedback を追加し（G1〜G3）、`render_home` が旧 `render_with_skeleton_frame` と同等の情報（mascot sidecar、sidebar 差分列、live terminal viewport、terminal feedback）を描けるようにする。
- pending shimmer: 旧経路の `skeleton_frame` カウンタは廃止し、`AppEvent::Tick` → `state.mascot_tick` に一本化（`HomeProjection` は既にこちらを使う）。tick は `EventPump` の既存 wakeup で供給される。
- `render_home` が内部で `Utc::now()` を取得している点は当面維持（既存挙動）。決定的テストが必要な箇所は strip + 相対時刻を含まない fixture で比較する（parity_suite の既存手法）。
- **暫定 shell overlay（G4）**: controller に相当がある modal（Overview / Notes / Environment / Remove / Quit / CreateSession）は controller の `Overlay` を使う。相当が無い PR modal / preview / error modal は、移行期間中 shell が `render_home` の出力に `*_modal::render_over` で重ねる（現行方式の流用）。Home 行 state には触れないため二重定義は生じない。controller への移設は後続 issue（§8）。

### 4.5 新フレームループ

`drive_workspace_with_agent_port_and_selection_mode` の後継（shell 名は `WorkspaceRuntime` とする）。

```rust
let mut state = AppState::home(workspace_id, session_ids);   // 初期 selected/active = root
let mut backend = DaemonBackend::new(...);
loop {
    // 1. 非同期完了・push を reducer へ
    let mut effects = Vec::new();
    for event in backend.drain_events() { effects.extend(update(&mut state, event)); }

    // 2. live 素材の更新（poll）と可用性同期
    backend.resize_panes(geometry);
    let live = backend.poll_terminals();                      // 旧 refresh_terminal 相当
    effects.extend(update(&mut state, AppEvent::LivePaneAvailability(live.has_pane)));

    // 3. 投影と描画
    let projection = HomeProjection::from_state(&state, name, cwd, &snapshot_sessions)
        .with_pane(backend.pane_state())
        .with_metrics(metrics_port.latest())
        .with_git_diffs(&git_diffs)
        .with_terminal_view(live.view);
    let mut frame = render_home(height, width, &projection);
    frame = shell_modals.render_over(frame);                  // G4 暫定（PR / preview / error のみ）
    term.draw(&frame)?;
    backend.drain_pane_launches(geometry);                    // 既存: pending 表示後に実起動

    // 4. 入力
    let key = term.read_key()?;
    if backend.forward_live_input(&state, &key) { continue; } // Closeup + live focus のみ PTY へ
    if let Some(event) = app_event_from_key(key) { effects.extend(update(&mut state, event)); }

    // 5. Effect 実行
    for effect in effects { if backend.dispatch(effect)? == Flow::Exit { return Ok(Exit::Quit); } }
}
```

1 フレームの順序（drain → poll → render → input → dispatch）は現行ループと同じで、`read_key` が tick / backend wakeup で戻る前提も不変。**変わるのは state の型と、入力・結果反映がすべて `update()` を通ること**。

### 4.6 削除対象

移行完了時（§5 の PR4）に削除するもの。

- 旧 `Workspace` view の row state 一式: `selected: usize`、`root_selected` / `new_session_selected` / `focused_session` / `pane_target` / `row_count` / `select_next/prev` / `selectable_rows` / `workspace_viewport_start` / `sidebar_row_at`、`mode`、`create_input` / `pending_session`、`panes` / `pane_documents`（名前キー投影）。
- `step_switch` / `step_closeup` / `step_closeup_tabs` / `apply_live_action` 等、旧 view を駆動する自由関数（reducer に同等あり）。
- `workspace::render` / `render_with_skeleton_frame` と `skeleton_frame`。
- `WorkspaceView::with_runtime_ids` 等のコンストラクタ（`AppState::home` + snapshot キャッシュに置換）。

---

## 5. 移行ステップ（PR 分割）

| PR | 内容 | 検証 |
|---|---|---|
| **PR1** 投影の parity | `HomeProjection` に `with_metrics` / `with_git_diffs` / `with_terminal_view` / feedback を追加し、`render_home` を旧 render と同等情報量にする。runtime は触らない | **parity golden**: 代表 state（empty / 多数 session / pending / live terminal / Closeup / tiny geometry / CJK）で旧 `render_with_skeleton_frame` と `render_home` の strip 済みフレームを比較するテストを追加し、差分ゼロ（意図差分は fixture 注記）を固定 |
| **PR2** Effect executor | `DaemonBackend`（`BackendPort` 本番実装）を新設し、`Effect` 全 variant の実行と `AppEvent` 還流を実装。`AgentRuntimeHost` を統合。runtime は未接続 | fake port（`SessionCommandPort` / `AgentCommandPort` 等の既存 fake）で variant ごとの dispatch → event 還流を unit test |
| **PR3** runtime 切替 | `WorkspaceRuntime`（§4.5）を実装し、`drive_workspace_with_*` の内部を差し替え。`app_event_from_key` / pointer hit-test（`HomeProjection::row_at` + `AppKey::SelectRow`）/ live input ゲートを接続。G4 modal は shell overlay として維持。**この PR で実端末経路が controller に切り替わる** | mod.rs の runtime テスト（約 70 個）を fake `Terminal` + fake port で新ループの期待値へ移植。parity golden（PR1）を新ループ経由で再検証。row contract（wrap / Enter / `t` / marker / viewport / empty / tiny）の integration test |
| **PR4** 旧経路の掃除 | §4.6 の削除。workspace.rs の旧 render 系テスト（27 箇所）を削除または `render_home` 系へ統合。`document/` 更新（02-architecture の TUI 経路記述）と issue #258 を `done` へ | full test + coverage 100%（削除でカバレッジ欠けが出ないこと）。lychee |

- PR1 / PR2 は独立しており並行可能。PR3 は両方に依存。PR4 は PR3 直後に最短で出し、二重実装の併存期間を最小化する。
- 切替はフラグを設けず PR3 で一発で行う（並存フラグは「記載＝実装済み」規約と二重テスト負担に反する）。退行時は PR3 の revert で旧経路に戻る。

---

## 6. テスト計画

- **reducer / 投影**: 既存の controller unit test + parity_suite を正とし、row contract（順序・初期 root・wrap・`+ new session` 活性禁止・marker 独立）はここに集約済み。PR1 で投影拡張分の unit test を追加。
- **parity golden**（PR1 → PR3 で再利用）: 旧 render と `render_home` のフレーム一致を固定してから切り替えることで、「切替＝描画退行なし」を機械的に保証する。golden は `crates/tui/tests/fixtures/` の既存方式（`home_cjk.golden`）に追加。
- **runtime integration**（PR3）: fake `Terminal`（scripted key 列 + フレーム capture）+ fake daemon port で新ループを end-to-end 駆動。シナリオ: 起動直後 root selected/active → j/k wrap → Enter で Closeup → Ctrl-O 復帰 → `+ new session` 活性 → create effect dispatch → `OperationResult` 反映 → live pane tab 移動 → Ctrl-C grace → quit。
- **live terminal 退行**（#295 / #305 保護）: PTY 出力 fixture を `TerminalSession` 経由で流し、`with_terminal_view` 投影がフレームに現れること、passthrough ゲートが controller state に従うことを検証。
- **tiny geometry / empty sessions**: `render_home` 既存テストに加え、新ループで 0/1 行 body・幅 1 での非 panic を integration で確認。
- カバレッジ 100% 維持: `DaemonBackend` は全 port 注入でテスト可能に作る。`#[coverage(off)]` は実 IO ラッパ（合成ルート側）に限定。

---

## 7. リスクと緩和

| リスク | 緩和 |
|---|---|
| live terminal 表示・入力の退行（#295 / #305 の done を壊す） | PR1 の `with_terminal_view` parity golden + PR3 の PTY fixture integration。passthrough ゲート条件を controller state 参照で 1 箇所に |
| テスト移植量が大きい（mod.rs 70 / workspace.rs 54） | PR 分割で段階化。PR3 時点で旧テストのうち「削除予定（PR4）」を明示的にマークし、移植対象を runtime 挙動テストに絞る |
| 二重実装の併存期間に仕様 drift | PR1 の parity golden を両経路に対して走らせ、drift を CI で検知。PR4 を PR3 直後に出す |
| `read_key` の tick 供給が止まると `OperationResult` の反映が遅延 | 現行ループと同じ `EventPump` wakeup に依存（挙動不変）。integration test で tick 駆動の還流を固定 |
| #287（create entry）との競合 | PR2 で `Effect::CreateSession` executor、PR3 で `+ new session` 活性化と `Overlay::CreateSession` が実端末に載る。#287 は残る Ctrl-A 動線と form UX の仕上げに縮小される見込み（着手前に scope を再確認） |
| 右ペイン tab 可視性の別 triage との競合 | tab layout は `render_home` の既存実装（#256 / #267）を無変更で使用。layout 変更を一切含めない |
| pointer 挙動の差 | hit-test を `HomeProjection::row_at` に集約し、viewport 計算を描画と共有。クリック選択は `AppKey::SelectRow` の reducer テストで固定 |

---

## 8. 後続 issue（本設計から切り出すもの）

1. **PR modal / preview overlay の controller 移設**（G4）: `Overlay::Prs` / `Overlay::Preview` と対応 `Effect` を追加し、shell 暫定 overlay を撤去する。
2. **pointer 語彙の controller 化**（G5 の恒久化）: `AppEvent::Pointer` を導入し、`AppKey::SelectRow` 変換を shell から reducer へ移す（terminal drag / copy は対象外のまま）。
3. **metrics / git diff の event 駆動化**（G2 / G3 の発展）: 毎フレーム polling を `BackendEvent` 化し、`DaemonBackend` の drain に統合する。

---

## 9. 参照

- issue: #258（本体）、#256 / #267（tab strip 投影）、#269 / #279 / #305（入力契約）、#286 / #295（live runtime 合成）、#293（marker 契約）、#287（create entry、`dependson: [258]`）
- 主要コード: `crates/tui/src/usecase/application/controller.rs`（`AppState` / `update` / `Effect` / `BackendPort`）、`crates/tui/src/presentation/views/workspace.rs`（`HomeProjection` / `render_home` / 旧 `Workspace`）、`crates/tui/src/presentation/mod.rs`（`WorkspaceUi` / `drive_workspace_*` / `step_*`）、`src/runtime/tui.rs`（合成ルート）、`crates/tui/tests/parity_suite.rs`
