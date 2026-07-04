---
number: 110
title: fix(mcp): session_delegate_issue は issue が委譲先の基点ブランチにコミット済みか検証する（未コミット issue がブランチに乗らない #104 系の根治）
status: todo
priority: high
labels: [orchestration, mcp, fix]
dependson: []
related: [104, 109]
parent: 105
created_at: 2026-07-04T21:46:55.648110+00:00
updated_at: 2026-07-04T21:46:55.648110+00:00
---

## 背景

`session_delegate_issue` は issue を `issue_to_prompt` でプロンプト化してから `session_create` するが、**新 worktree は基点ブランチ（既定 `main`）の HEAD から切られる**ため、未コミットの issue ファイル（例: root/workspace root に作られ `main` に未マージ）は新 worktree の枝に乗らない。プロンプト本文には issue の内容が埋め込まれるので着手はできるが、その session が当該 issue の `status` を更新しようとしても**自分の worktree に issue ファイルが存在しない**ため、`done` を立てられず、あるいは新規ファイルを作って番号が二重化する。#104 が踏んだのはこれ（root が `main` で issue を定義 → 未マージのまま委譲 → 枝に乗らない）。

新しい運用モデルでは issue は必ずトリアージ session（#109）経由でコミット → マージされてから root が委譲するので、正常フローでは基点に乗っている。本 issue はその**前提をツール側で検証**して早期に気付けるようにする（誤運用の握り潰しを防ぐ）。

## やること

- `session_delegate_issue` で、委譲先 worktree の**基点コミットに issue ファイルが含まれるか**を検証する（基点解決は既存 `resolve_base_ref`。基点ツリーに `.usagi/issues/<file>` があるか）。
- 含まれない場合はツールエラーで「この issue はまだ基点ブランチにコミットされていないため委譲先の枝に乗らない。トリアージ session で起票・マージしてから委譲すること（または `session_delegate_brief` を使う）」と案内する（黙ってプロンプトだけ渡さない）。
- 実装方針の選択肢を検討し、トレードオフを設計 proposal に沿って決める:
  - (a) 検証のみ（推奨・最小）: 未コミットなら拒否。運用（トリアージ→マージ→委譲）で担保する。
  - (b) 自動搬送: 新 worktree に issue ファイルをコピーして初回コミットする。→ provenance が濁る／二重管理になるため非推奨。
- ユニットテスト: 基点にコミット済み issue は委譲成功、未コミット issue は拒否。

## 受け入れ条件

- コミット済み issue の委譲は従来どおり成功し、委譲先 worktree に issue ファイルが存在する（session が自枝で `status` を更新できる）。
- 未コミット issue の委譲は明確なエラーで拒否され、原因と対処が案内される。
- ドキュメント（[03-commands/03-mcp.md](../../document/03-commands/03-mcp.md) の `session_delegate_issue` の挙動）に前提と検証を追記する。
