---
number: 436
title: refactor(tui): views/config.rs の SavePhase 状態機械・保存実行・モデル差し替えポリシーを usecase 層へ移設する
status: todo
priority: medium
labels: [refactor, tui, review]
dependson: []
related: []
created_at: 2026-07-20T12:01:23.251822+00:00
updated_at: 2026-07-20T12:01:23.251822+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

`crates/tui/src/presentation/views/config.rs`（834 行）に、view の責務を超えるロジックが同居している:

- `:25-34` — `enum SavePhase { Idle, Saving, Saved }` の状態機械。
- `:128-155` — `load_with_available_models` が available models に応じて `default_model` を差し替えるポリシー（swap 判定 :136-150）。
- `:298-305` `begin_save`、`:311-331` `commit_save(&mut self, port: &mut dyn SettingsPort)` — view が port へ保存を実行し SavePhase を遷移させる。

## 問題

Home/New と揃えたアーキテクチャ（reducer が状態、view は描画のみ）から config だけ外れており、保存の状態遷移・ポリシーが view テストでしか固定できない。

## 改善案（要検討）

- SavePhase・保存実行・モデル差し替えポリシーを usecase 層（controller/reducer 側）へ移設し、view は描画に徹する。
- Home/New と同世代のアーキテクチャに揃える。

## 受け入れ条件

- [ ] config.rs から状態機械・port 呼び出し・ポリシーが消え、usecase 側でテストされている。
- [ ] Config Save の UX（loading→saved→自動復帰、#397 の成果）が回帰しない。
- [ ] coverage 100% を維持する。
