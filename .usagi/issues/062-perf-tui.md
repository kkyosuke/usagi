---
number: 62
title: perf(tui): 毎フレーム・毎打鍵の無駄な再計算/再確保を削減する
status: done
priority: low
labels: [perf, tui, review]
dependson: []
related: [40, 41]
created_at: 2026-06-19T22:17:07.364869+00:00
updated_at: 2026-07-11T01:18:56.132387+00:00
---

## 背景

TUI の描画ループで、結果が変わらない処理を毎フレーム・毎打鍵で再計算/再確保している箇所がある。差分描画のため実害は限定的だが、セッション数が増えると効いてくる。

- **`focus_menu_commands()` の毎回再構築**（`src/presentation/tui/home/state/mod.rs:938-975`）— 静的なコマンド集合に対し `registry.commands_in_scope(Session)`（`infos()` が全メタデータを clone）をフレーム毎・キー毎に呼ぶ（`focus_menu_move_up/down`・`focus_selected_command`・`focus_menu`・`switch_preview` から）。→ 一度算出してキャッシュする。
- **`flush` の base フレーム全体 clone**（`src/presentation/tui/screen.rs:244-251`）— インストール非実行時（大半）でも `self.base.clone()` してオーバーレイを当てている。→ `install_task::snapshot()` が `None` のときは clone せず `&self.base` を直接 diff へ渡す。
- **`dim_row` の描画→ANSI strip**（`src/presentation/tui/home/ui/panes.rs:243-247`）— Switch の非選択行を毎フレーム「render → `strip_ansi_codes`（行毎アロケート）→ 再 style」している。→ dim 版を直接構築する。
- **store の write 毎の全件 rebuild**（`src/infrastructure/issue_store.rs:142`、`src/infrastructure/memory_store.rs:126`）— `write`/`remove` のたびに `rebuild_index()`→全 md を再スキャンして index 全体を書き直す。1 件更新でも全件再読込。→ incremental 更新を検討（件数が小さい前提なら優先度低）。

## 確認方法

- 描画結果が変わらないこと（既存の ui テスト維持）。
- カバレッジ 100% 維持。

関連: #40 / #41（永続化・軽微 perf の既存まとめ）。
