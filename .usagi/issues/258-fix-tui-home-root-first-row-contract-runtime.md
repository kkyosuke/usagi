---
number: 258
title: fix(tui): Home の root-first row contract を runtime まで一元化する
status: in-progress
priority: high
labels: [tui, bug, triage]
dependson: []
related: [238, 245]
created_at: 2026-07-12T23:37:34.688708+00:00
updated_at: 2026-07-12T23:52:22.101056+00:00
---

## 目的

v2 TUI の Home 左ペインで、実行時に表示・操作される全経路を **root → sessions → + new session** の 1 つの row contract に統一する。root は workspace を開いた直後の selected / active target であり、表示順・入力・viewport・marker がこの不変条件からずれないようにする。

## 調査結果

- controller の `AppState::rows()` と `HomeProjection::rows()` はすでに `root → sessions → + new session`。`AppState::home()` の初期 selected / active も root で、`move_selection()` はこの row 列を基準に wrap する。
- ただし実 runtime は `presentation::run_workspace()` → `WorkspaceUi` → `workspace::render()` / `step_switch()` であり、controller の `HomeProjection` / `render_home()` は runtime に接続されていない。
- その旧 `Workspace` state/render は `sessions → root` を保持する。selected index、`root_selected()`、`focused_session()`、`selectable_row()`、viewport と `step_switch()` の上下移動が root 末尾の index contract に依存しており、controller と同じ画面を二重に定義している。
- 右ペイン tab 非表示を扱う別 triage はこの二重描画経路に触れる可能性がある。本 issue は Home の row state / runtime 接続を扱い、tab の可視性・right-pane layout の仕様変更は扱わない。該当 issue が起票されたら non-blocking related に相互リンクする。

## スコープ

- runtime が controller の `AppState` と `HomeProjection` を唯一の Home state / render source として使うよう、`run_workspace` の入力・描画・イベント dispatch を接続する。または旧 `Workspace` 経路を削除して同等の runtime seam を controller 側に集約する。
- Home の selectable rows を root、snapshot order の sessions、`+ new session` の順だけで定義し、navigation・render・viewport が同じ列を参照する API にする。index で session/root を推測する旧 contract を残さない。
- 初期状態は root が selected と active。selected cursor（`>`）と active marker（`*`）は別概念のまま、root / session / `+ new session` に正しく表示する。
- Switch で `↑/↓` と `j/k` は row 列を循環する。root から上は `+ new session`、`+ new session` から下は root。Enter / `t` は target 行のみ active にして Closeup へ入り、`+ new session` では作成 effect を dispatch する。
- 既存または追加する jump key（例: `G`）があれば、row contract 上の意味を明文化して実装・テストする。導入しない場合は未対応キーを navigation として解釈しない。
- viewport は選択行を必ず含め、root-first の先頭表示と末尾 `+ new session` の wrap の双方で正しい範囲を表示する。0/1 行 body、狭幅など tiny geometry でも panic / overflow しない。
- session が 0 件でも root と `+ new session` を表示し、root 初期 selected/active、navigation、作成 action を維持する。
- runtime/integration と reducer/render の回帰テストを追加し、旧 render 経路にだけ通るテストを controller-connected runtime の期待値へ移す。

## 対象外

- 右ペインの tab を表示するか隠すか、tab の layout / input semantics を変更しない。
- session lifecycle、daemon snapshot transport、terminal / agent pane の機能追加は行わない。
- root-first 以外の sidebar visual redesign は行わない。

## 完了条件

- 実端末で用いられる Home 描画・入力経路が controller projection を経由し、旧 `Workspace` の sessions-first/root-last state が実 runtime の source of truth として残らない。
- 期待順序は常に `root → sessions (snapshot order) → + new session` で、初期 selected / active は root。
- `↑/↓/j/k` の wrap、Enter / `t`、存在する jump key の意味が row contract に対してテストされる。
- selected と active が異なる場合も `>` と `*` がそれぞれ正しい行に出て、`+ new session` が active になることはない。
- 多数 session の viewport、empty sessions、0/1 body と狭幅を含む tiny geometry で、選択行の可視性・行数・幅制約を回帰テストする。
- right-pane tab 非表示の別 issue と同一の tab/layout 変更を含めず、両方が存在する場合に競合しない seam を保つ。

## 検証

- controller reducer の row order / initial selection / wrap / activation tests。
- `render_home` と runtime frame の golden または integration tests（marker、viewport、empty、tiny geometry）。
- fake terminal と real PTY regression で、実 runtime の root-first navigation と frame を確認する。
