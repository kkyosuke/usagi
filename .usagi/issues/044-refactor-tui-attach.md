---
number: 44
title: refactor(tui): ホーム画面の attach フロー集約とデッドパラメータ・コールバック整理
status: todo
priority: medium
labels: [refactor, tui]
dependson: []
related: []
created_at: 2026-06-18T22:40:31.332638+00:00
updated_at: 2026-06-18T22:40:31.332638+00:00
---

## 背景

ホーム画面の event/handler 層に、フロー上の同一概念が抽象化されず手書き反復されている箇所がある。`#[allow(clippy::too_many_arguments)]` が量産される主因にもなっている。

### 1. 「focus してライブなら attach」が 3〜4 か所に逐語コピー（高）
`event/handlers.rs` の `switch_key`（Enter/l 分岐）・`leave_switch`（`ReturnMode::Attached`）・`activate_named` が、`state.enter_focus(row)` → `preview()` 確認 → `open_pane(...)` を毎回手書きしている。

### 2. `open_pane` の引数 `_reader` / `_preview` が完全なデッドパラメータ（高）
`event/handlers.rs` の `open_pane` は「シグネチャを揃えるため」に `_reader`/`_preview` を受け取って未使用。さらに `run_focus_command` / `focus_menu_key` / `focus_prompt_key` も、それらを `open_pane` に素通しするためだけに引きずっている。

### 3. `event_loop` のコールバックが 14 引数（高）
`event/mod.rs` の `event_loop` は 14 引数（うち 8 個が `&mut dyn FnMut`）。`mod.rs` の `run` で全てローカルクロージャとして定義し、`workspace.path.clone()` を `remove_root`/`rename_root`/`terminal_root`/`config_root`/`branches_root` と複数回別名でクローンしている。

## 改善方針

- `focus_and_maybe_attach(...)` 1 関数に「focus → ライブなら attach」を集約し、3 か所から呼ぶ。pane は自前で入力を読むため `reader` の素通しは不要になる。
- `open_pane` から `_reader`/`_preview` を削除。集約後に不要になった `too_many_arguments` allow を外す。
- セッション操作系コールバック（create / rename / remove / existing_branches）を 1 つの trait（例 `SessionActions`）にまとめ `&mut dyn SessionActions` 1 引数で渡す。`root` のクローンも構造体に 1 本化する。テストはモック 1 個で差し替え可能になる。

## 確認方法

- 切替/没入/コマンド経由での attach 挙動が従来どおりであること（event テスト・E2E）。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
