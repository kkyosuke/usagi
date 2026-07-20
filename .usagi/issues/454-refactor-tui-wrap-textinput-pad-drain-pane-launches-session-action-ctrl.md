---
number: 454
title: refactor(tui): 写経コードの束（wrap カーソル・TextInput 委譲・pad 再実装・drain_pane_launches・session action 三重定義・Ctrl 判定）
status: todo
priority: low
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T12:05:28.095659+00:00
updated_at: 2026-07-20T12:05:28.095659+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。単独では小粒な「写経（コピペ・同型再実装）」をまとめて解消する。

## 根拠と改善案（要検討・検証済み）

1. **wrap カーソル演算**: `(i + len - 1) % len` / `% len` の同型演算が view 約 10 ファイル（overview_modal / closeup_modal / pr_modal / workspace / welcome / new / open ほか）に散在。→ `WrappingCursor` ヘルパへ。
2. **TextInput 1 行委譲**: `widgets/text_input.rs` は pub fn 22 個で、呼び出し側に同型の委譲・キー分岐が多数。`workspace_runtime.rs:144-228` の `handle_overview_key` / `handle_closeup_key` は Up/Down/Left/Right/Home/End/Delete/Select*/Backspace/Tab/Char/Enter/Escape の **~16 アームがイベント variant 名以外同一**。`presentation/mod.rs` の `step_new`（:950）/`step_open`（:1065）も同型。→ `EditKey` enum ＋ `apply(input, EditKey)` に集約し、variant 写像だけ残す。
3. **pad_to_width 再実装**: 正本 `widgets/mod.rs:274` に対し、`views/open.rs:374` `fit()`・`views/welcome.rs:342` `pad_segment()` が再実装。→ 正本へ委譲。
4. **drain_pane_launches**（`presentation/mod.rs:1306`）: `launches.remove(0)`（:1309、O(n) pop-front）と `PaneLaunch::Agent`（:1310）/`PaneLaunch::Terminal`（:1339）の 2 アーム丸写し。→ VecDeque 化＋アーム共通化。
5. **session action マッピング三重定義**: `crates/cli/src/cli/mod.rs:197-211` と :363-379（CLI 内で 2 回）、`crates/cli/src/mcp/serve.rs:338-344`、`crates/tui/src/usecase/application/lifecycle_adapter.rs:626` — `SessionAction` と payload 構築が分散。→ payload 構築を core（`usecase/client.rs` の語彙側）へ。
6. **Ctrl 判定同型コード**: `crates/tui/src/usecase/terminal_input.rs:280/:285/:290/:295/:300` — `matches!(key.code, KeyCode::Char('x')) && is_only_control(key.modifiers)` の 5 連コピー。→ `ctrl_chord(key, 'x')` ヘルパへ。

## 受け入れ条件

- [ ] 6 項それぞれについて共通化または見送り理由の記録がされている。
- [ ] 挙動が回帰しない（既存テスト維持）。coverage 100% を維持。
