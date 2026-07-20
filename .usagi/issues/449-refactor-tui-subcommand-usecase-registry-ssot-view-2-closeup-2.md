---
number: 449
title: refactor(tui): コマンドパレットの subcommand 表を usecase registry に SSoT 化する（view 2 ファイル＋closeup 内 2 箇所のハードコード解消）
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: [374, 412]
created_at: 2026-07-20T12:04:33.182683+00:00
updated_at: 2026-07-20T12:04:33.182683+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。**#374（modal の形別コンポーネント整理、マージ済み）は palette の描画・組版を共通化したが、subcommand データの SSoT 化は未実施**であることを現行コードで確認した。

## 根拠（検証済み）

- 両 view はトップレベルコマンドを usecase registry（`overview::complete`/`overview::help`）から取る一方、**subcommand は view にハードコード**:
  - `crates/tui/src/presentation/views/overview_modal.rs:324-327` `subcommands()` → `Some("session") => &["list","overview","remove"]`。
  - `crates/tui/src/presentation/views/closeup_modal.rs:239-243` `subcommands()` → `"close" => &["--force"]`、`"terminal" => &["open","new"]`。
  - `closeup_modal.rs:258-260` `subcommand_completion()` — **同じ表をもう一度**ハードコード。
- usecase registry 側には既に同じ情報がある: `crates/tui/src/usecase/closeup/commands/close.rs:24`・`terminal.rs:24` の `arguments: "--force"` / `"new"`。

## 問題

registry にコマンド/引数を追加しても、view 側の補完表を 3 箇所（overview 1＋closeup 2）手動更新しない限り**補完が黙って効かない**（SSoT 違反）。

## 改善案（要検討）

- registry の `CommandInfo` に subcommand（補完候補）を持たせ、view はそれを参照するだけにする。
- closeup 内の 2 箇所（`subcommands` / `subcommand_completion`）は 1 参照に統合する。
- パレット状態の共通型化は #374 の成果（widgets/modal/palette.rs）の上で行う。

## 受け入れ条件

- [ ] subcommand 情報の定義箇所が registry の 1 箇所になり、view のハードコード表が消える。
- [ ] registry へのコマンド追加が補完に自動反映されることがテストで固定されている。
- [ ] coverage 100% を維持する。
