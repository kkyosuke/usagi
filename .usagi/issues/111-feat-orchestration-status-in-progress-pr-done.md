---
number: 111
title: feat(orchestration): status ライフサイクルの単一書き手化 — 委譲プロンプトに「着手で in-progress・PR 前に done を自枝で」を組み込む
status: done
priority: high
labels: [orchestration]
dependson: []
related: [100, 104]
parent: 105
created_at: 2026-07-04T21:47:21.070475+00:00
updated_at: 2026-07-04T23:12:19.549006+00:00
---

## 背景

規約は「issue の `status` を書くのはその issue を担当する session だけ、root は書かない」（[.agents/workflow.md](../../.agents/workflow.md)）。しかし session がマージ後に `session_remove` されると誰も `done` にせず、#104 は #615 でマージ済みなのに `todo` に取り残された。root は原則 2 で status を書けないため、**session が生きているうちに自枝で `done` を立て、PR（マージ）で `main` に `done` を運ぶ**のが唯一整合する経路。この指示を委譲プロンプトに確実に組み込む。

あわせて `in-progress` の扱いを設計 proposal に合わせて確定する: `main` の backlog に `in-progress` が乗るのはマージ後（＝実際は完了後）で遅いため、**root は「session が存在するか」を in-progress の実効シグナルとして使う**（`session_status` / 命名規約 `issue-<番号>`）。issue ファイルの `in-progress` は当該 session 内のローカル進捗表現に留める。

## やること

- `issue_to_prompt` が生成するプロンプトの status 指示を次に明確化する（リポジトリ非依存の文言のまま）:
  - 着手時: 自 worktree で `status = in-progress`。
  - **PR を開く前に**: 自 worktree で `status = done` にコミットし、その差分を PR に含める（マージで `main` に `done` が乗る）。
  - status 差分は実装差分と同じブランチ・同じ PR に載せる（別コミットで可）。
- root 側の運用として、`session_status` の `merged` と session インベントリ（`session_list`）から「in-progress = 生存 session あり」「done = 基点にマージ済み」を判定する手順を明文化する（root は status を書かない）。
- マージ後に `done` が取り残される既知ケースへの対処方針を決める（一次対策は上記の自枝 done。取りこぼしの是正は、`done` 化 PR を出す軽量なクローズ session に委ねるか、将来のマージ検知連動に譲るかを proposal で比較し、当面は運用で吸収する）。

## 受け入れ条件

- 委譲された session が、実装 PR に issue の `done` 差分を含めて出し、マージで `main` の当該 issue が `done` になる（人手・root の書き込み不要）。
- root は status を一切書かずに、`session_status` / `session_list` だけで in-progress・done を判定して次の委譲へ進める。
- ドキュメント（[.agents/workflow.md](../../.agents/workflow.md) の status ライフサイクル、[03-commands/03-mcp.md](../../document/03-commands/03-mcp.md) の `issue_to_prompt`）を更新する。
