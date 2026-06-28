---
number: 79
title: feat(tui): unite導線 — Open画面の複数選択＋直近統合セット記憶
status: todo
priority: medium
labels: [feat, tui]
dependson: [78]
related: []
parent: 77
created_at: 2026-06-28T00:08:17.128202+00:00
updated_at: 2026-06-28T00:08:17.128202+00:00
---

親 #77 のフェーズ2。Open 画面で統合対象を選ぶ導線。

- Open 画面を複数選択化（`Space` で行トグル、`Enter` で選択中をまとめて開く）。選択 0 件のときの `Enter` はカーソル行 1 件として扱う。
- 1 件 → 従来どおり単一ホーム、2 件以上 → 統合ホーム（複数グループの `HomeState` を構築）。
- 直近の統合セットを記憶（`resume-focus` と同方式のグローバルスナップショット）し、次回 Open で復元/プリ選択。
- ホーム構築側（`home/mod.rs` の `run`/`preload`）を複数ワークスペース対応に拡張。`event::Wiring.workspace_root` を行のグループから解決する形へ。

## 確認方法

- 単一選択は挙動不変。複数選択で統合ホームが開く。直近セットが復元される。
- `cargo fmt` / `clippy` / `test`（カバレッジ 100%）。
