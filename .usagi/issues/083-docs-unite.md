---
number: 83
title: docs: 統合(unite)モードのドキュメント整備
status: todo
priority: medium
labels: [docs]
dependson: [79, 80, 81, 82]
related: []
parent: 77
created_at: 2026-06-28T00:08:38.267677+00:00
updated_at: 2026-06-28T00:08:38.267677+00:00
---

親 #77 のフェーズ6。実装に追従してドキュメントを更新（実装と同じ各 PR で部分更新しつつ、最終整合をここで取る）。

- `document/design/home/`（モード・レイアウト・サイドバー）に統合表示（グループヘッダ・カーソル飛ばし・スコープ解決）を追記。
- `document/design/02-open.md` に複数選択の導線を追記。
- `document/04-orchestration.md` に「複数ワークスペースの統合」概念を追記。
- `document/data/01-global.md` に直近統合セットのスナップショット仕様を追記。
- 必要なら `README.md`。
- ドキュメント規約（記載＝実装済み・SSoT・相対リンク/アンカー整合）に従う。lychee/markdown-link-check を通す。
