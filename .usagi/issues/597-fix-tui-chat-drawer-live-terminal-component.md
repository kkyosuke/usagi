---
number: 597
title: fix(tui): chat drawer の live terminal 描画を右ペインと同じ component に統一する
status: in-progress
priority: high
labels: [tui, bug, terminal, agent, refactor]
dependson: []
related: [576, 577, 596]
created_at: 2026-07-31T11:05:30.567509+00:00
updated_at: 2026-07-31T11:13:15.283229+00:00
---

## 症状

chat drawer（指示モード）で codex / claude を動かすと、表示領域が壊れる。

- 出力を drag 選択すると **表示が scrollback の先頭（最古）に飛ぶ**。live screen の位置がずれるため、
  codex の描画と入力欄が重なって見える。
- 選択・copy すると `copied 3 lines` のような **feedback がターミナル本文の 1 行として描かれる**。
  右ペインでは同じ feedback は footer に出る。

## 原因

drawer と右ペインが、同じ `TerminalViewProjection` から**別の描き方**をしている。

右ペイン（`views/workspace.rs` の `home_right_pane`）:

- `terminal_viewport_rows(&view.rows, view.row_offset, view.total_rows, view.scroll, width, cap)` で
  **下端 anchor の window** を切り出す。
- `view.feedback` は footer 行に描く（`TerminalViewProjection::feedback` の doc も「footer に表示する」と定義している）。

drawer（`views/workspace_agent_drawer.rs` の `drawer_body`）:

- projection が `terminal_rows: Vec<String>` しか持たず、`row_offset` / `total_rows` / `scroll` を受け取っていない。
- `terminal_rows.iter().take(content_capacity)` と**先頭から**切るだけで、window 化も scroll 反映もしない。
- `presentation/mod.rs` の `workspace_agent_drawer_projection` が `view.feedback` と interrupted detail を
  `terminal_rows` へ **push** している。つまり feedback が本文行になる。

`controller_terminal_view` は 2 経路を返す。

| 経路 | `view.rows` の中身 |
|---|---|
| 選択なし | `visible_range` で window 化済み（行数 = viewport 行数） |
| **選択あり** | `display_rows_with_scrollback_selection` の **retained 全行**（scrollback + grid） |

drawer の `take()` は前者では偶然正しく、後者では最古の行を描く。これが「選択すると表示が壊れる」の直接原因である。
また `rows_with_scrollback` は末尾の空行を落とすため、行数が viewport より短いフレームでは push された
`copied N lines` が content 領域に収まって描かれる。

さらに `workspace_agent_drawer::terminal_point_at` は `start = rows_len - (viewport.rows + scroll)` という
**下端 anchor 前提**で hit-test している。つまり同一ファイル内で renderer と pointer 変換が食い違っており、
`total_rows > viewport.rows` のフレームでは drag が別の cell を選ぶ。

## 変更方針

**live terminal viewport を 1 つの component にして、右ペインと drawer が同じものを使う。**

- `TerminalViewProjection` を描く処理（下端 anchor window + width clip + footer feedback）を共有 widget へ抽出する
  （置き場所は `presentation/widgets/` 配下。`views/workspace.rs` の `terminal_viewport_rows` /
  `RIGHT_PANE_CONTENT_TOP` / `RIGHT_PANE_FOOTER_GAP` が現行の正本なので、そこから移す）。
- `WorkspaceAgentDrawerProjection.terminal_rows: Vec<String>` を
  `terminal_view: Option<TerminalViewProjection>` に置き換える。`workspace_agent_drawer_projection` は
  `view.feedback` を本文へ push せず、そのまま渡す。
- drawer は footer 行に feedback を描く（feedback が無いときは現行の key hints を維持する）。feedback と key hints の
  どちらを優先するかを決めて明記する（右ペインは feedback 優先）。
- interrupted detail（`focused_interrupted().safe_detail()`）は terminal 出力ではないので、本文行への push を
  やめて専用の表示位置へ置く。
- renderer と `terminal_point_at` が同じ window 計算を共有する（geometry を 2 箇所で持たない）。
- drawer の viewport 行数・桁数の計算（`terminal_viewport`）は現状どおり drawer 専用のままでよい。共有するのは
  **中身の描き方**であって外枠の geometry ではない。

## 対象ファイル

- `crates/tui/src/presentation/views/workspace_agent_drawer.rs`
- `crates/tui/src/presentation/views/workspace.rs`（`home_right_pane` / `terminal_viewport_rows` の抽出）
- `crates/tui/src/presentation/widgets/`（共有 widget の新設）
- `crates/tui/src/presentation/mod.rs`（`workspace_agent_drawer_projection`）
- `document/03-tui.md`（drawer の viewport 記述に「本文は右ペインと同じ window / feedback は footer」を明記）

## 受け入れ条件

- drawer で選択がある / ない両方のフレームで、描かれるのは **live bottom に anchor した window** である
  （`total_rows > viewport.rows` の projection で最古行が出ない回帰テスト）。
- `Ctrl-O u` / `Ctrl-O d`（↑↓）の scroll が、選択があるフレームでも drawer に反映される。
- copy / 選択 / link の feedback が drawer 本文行に混ざらず、footer に出る。
- interrupted detail が terminal 出力行として描かれない。
- `terminal_point_at` の hit-test 結果が、実際に描かれた行と一致する（renderer と同じ window から導出されている）。
- 右ペインの既存 frame（tab strip・footer・scroll）は不変である。

## テスト方針

- `cargo test -p usagi-tui presentation::views::workspace_agent_drawer`
- `cargo test -p usagi-tui presentation::views::workspace`（共有 widget 抽出後も右ペインの frame が不変であること）
- `cargo test -p usagi-tui presentation`（`workspace_agent_drawer_projection` が feedback を本文へ入れない seam test）

## 非目標

- drawer の外枠 geometry（幅 60% / 56–96 桁 / 全幅縮退）の変更。
- conversation selector の UI 変更。
- 入力・attach 周りの修正（#596）。
