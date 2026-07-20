---
number: 456
title: docs: ドキュメント・規約ドリフトの束（虚偽/stale コメント・依存表の toml 欠落・module doc 不足・文字数基準の切り詰め等）
status: todo
priority: low
labels: [docs, review]
dependson: []
related: [410]
created_at: 2026-07-20T12:06:01.468007+00:00
updated_at: 2026-07-20T12:06:01.468007+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。doc と実装のドリフト、および表示幅規約（display_width）からの小逸脱を束ねる。

**注**: レビュー時に指摘された `crates/cli/src/mcp/serve.rs` 冒頭の stale doc（「どの tool も未実装エラー」）は #1135/#1136 のマージで**解消済み**のため対象外。daemon_backend.rs の虚偽 doc は #410 の決定とセットで扱う（ここでは参照のみ）。

## 根拠（検証済み）と対応

1. `crates/tui/src/presentation/views/workspace.rs` — 存在しない legacy 関数への参照コメント **2 件**: :889（`right_pane` — 実在せず、あるのは `home_right_pane` :1512）と :1121（`left_pane` — 実在せず）。:865-866 に本文のない孤立セクション見出し。（レビュー当初の 6 件中、:1135/:1413/:1558 は実在コードへの言及で問題なし。）→ コメント修正・見出し削除。
2. `document/06-conventions.md` 依存表（:40-55）— ルート `Cargo.toml:77` の `toml = "0.8"`（usagi-tui の allowlist 解析用、crates/tui/Cargo.toml:20）が**未記載**。→ 表に追加。
3. `crates/core/src/usecase/mod.rs:5-14` — doc が 5 モジュール（issue/memory/note/session/workspace）しか説明せず、実際は 9（agent/client/pr_inventory/settings が漏れ）。→ doc 更新。
4. `crates/tui/src/presentation/views/overview_modal.rs:20-22` — `MAX_MATCHES = 8` と `BODY_HEIGHT = 16` が不整合: `body()`（:346-405）は固定 chrome ~9 行＋候補 ≤8 行 = 最大 17 行で、候補が多いと footer が黙って切れる（closeup_modal.rs:18-20 も同ペア）。→ const assert か BODY_HEIGHT の導出化。
5. `crates/tui/src/presentation/widgets/mod.rs:236-263` — `wrap_to_width` は ANSI 非対応（`text.chars()` を素で数える）だが doc（:236-238）に明記なし（隣の `pad_to_width` :274 は明記あり）。→ doc に制約を明記。
6. `crates/tui/src/presentation/widgets/select.rs:30-31, 38-39` — `format!("{label:<WIDTH$}")` による **char 数パディング**（CJK で列ずれ）。→ `pad_to_width`（display 幅基準）へ。
7. settings.json だけ version envelope なし: `store/workspace.rs:122`（`load_settings` 素 read）/:133（`save_settings` 素 write）。同ファイルの workspaces.json は `read_versioned`/`write_versioned`（:96, :108）、state.rs（:66/:76）・memory.rs（:96）・issue.rs（:92）も version 付き。→ versioned 化（migration 込み）。
8. `crates/tui/src/presentation/mod.rs:873` — `step_config` の引数 `_settings: &mut dyn SettingsPort` が未使用（body :874-903 は config と key のみ）。→ 削除（config 移設 issue #436 と重なる場合はそちらで）。
9. `crates/tui/src/presentation/mod.rs:1032-1047` — `new_project_notice` が `detail.chars().count() > MAX`（:1041）/`chars().take(MAX - 1)`（:1042）の**文字数基準**切り詰め（CJK で表示幅超過）。→ `display_width` 基準へ。

## 受け入れ条件

- [ ] 各項の doc/コメント/表が実装と一致し、コード側の小修正（4/6/7/9）はテストで固定されている。
- [ ] Markdown link check green。coverage 100% を維持（コード変更分）。
