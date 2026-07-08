---
number: 109
title: feat(mcp): ブリーフ起点の起源フロー — session_delegate_brief（事前 issue 不要でトリアージ session を起こす）
status: done
priority: high
labels: [orchestration, mcp]
dependson: []
related: [104]
parent: 105
created_at: 2026-07-04T21:46:32.154417+00:00
updated_at: 2026-07-08T22:37:13.197531+00:00
---

## 背景

原則 2 で root は issue を作れない（issue ファイルは git 追跡下＝repo 変更）。ではどこから作業が生まれるか。答えは「root は**自由記述のブリーフ**を新規 session に渡し、その session が worktree 内で調査 → issue 起票 → PR する（トリアージ/設計 session）」。issue はこの session のブランチに乗り、マージで `main` の backlog に現れ、以降 root が `session_delegate_issue` で委譲できる。

既存でもこれは `session_create` + `session_prompt(mode=queue)` の 2 手で実現でき、`session_prompt` は issue を要求しない。この定番手順を 1 tool にまとめ、`session_delegate_issue` と対になる「起源」の入口を明示する。

## やること

- MCP に `session_delegate_brief`（名前は要検討）を追加する。引数: `brief`（自由記述の指示・必須）、`name`（session 名・任意、既定は `triage-<連番>` などの安全名）。挙動は `session_create(name)` → `session_prompt(name, brief, mode=queue)` を順に呼ぶだけ（新ロジックを足さない。`session_delegate_issue` と同じ合成パターン）。
- ブリーフには「調査結果を issue として起票し、実装は別 issue に分割してよい／このブランチで PR する」という**トリアージ session としての定型指示**をラップして渡す方針を決める（system prompt 側の worktree 閉じ込め指示と両立）。
- 返り値は `{ session, root, worktrees, delivered_to }`（issue 番号は無い）。
- root ガード（#106）では `session_*` を許可済みなので、この tool は root から呼べる。
- ユニットテストで create→queue の合成を網羅（既存 `delegate_issue_*` テストと同型）。

## 受け入れ条件

- root から `session_delegate_brief(brief: "...")` を呼ぶと、事前 issue なしで新規 session が作られ、ブリーフが起動時キューに積まれる（`autostart_queued_prompts` ON なら自動 spawn で着手）。
- そのトリアージ session が worktree 内で `issue_create` して PR し、マージ後 root の `issue_search` に現れる。
- ドキュメント（[03-commands/03-mcp.md](../../document/03-commands/03-mcp.md) / [04-orchestration.md](../../document/04-orchestration.md)）に起源フローを追記する。
