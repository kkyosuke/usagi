---
number: 82
title: feat(tui): unite動的操作 — コマンドパレットの unite add/remove
status: todo
priority: low
labels: [feat, tui]
dependson: [81]
related: []
parent: 77
created_at: 2026-06-28T00:08:33.451535+00:00
updated_at: 2026-06-28T00:08:33.451535+00:00
---

親 #77 のフェーズ5。統合中にワークスペースを動的に足し引きする。

- コマンドパレットに `unite add <workspace>` / `unite remove <workspace>` を追加。グループの追加/削除でサイドバーを再構成し、直近統合セットの記憶も更新。
- `unite remove` で残り 1 件になったら単一ホーム表示へ自然に戻る（ヘッダが消える）。

## 確認方法

- add/remove でグループが増減し、各種背景監視・プールが破綻しない。
- `cargo fmt` / `clippy` / `test`（カバレッジ 100%）。
