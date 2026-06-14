---
number: 023
feature: issue-store
title: issue ストア（`.usagi/issues/` への永続化と採番）
status: todo
priority: high
category: core
dependson: []
ref: —
---

# issue ストア（タスク issue の永続化基盤）

## 概要

usagi で管理するプロジェクトのタスクを「issue」として永続化する基盤を追加します。保存先は `<repo>/.usagi/issues/`。1 issue = 1 ファイルで、本リポジトリの issue 群と同じ **frontmatter 付き markdown**（`NNN-feature.md`）形式とします。検索を高速化するため、frontmatter のメタデータを抽出した **JSON インデックス**（`.usagi/issues/index.json`）を併設します。

issue はチームで共有したい情報のため、`.usagi/` 配下でありながら **git 管理対象に含めます**。`.usagi/` の他のファイル（`state.json` / `history.json` / `settings.json` / `worktree/`）はマシンローカルのままなので、`.gitignore` を「共有しないものだけ無視する」選択的パターンへ変更します。

本 issue はドメインモデルと永続化層（infrastructure / usecase）までを対象とし、CLI / MCP の公開インターフェースは [024-issue-cli](024-issue-cli.md) / [025-issue-mcp](025-issue-mcp.md) で扱います。

## issue のデータ構造

frontmatter は既存 issue 形式を踏襲しつつ、タスク管理に必要な項目を持ちます。

```markdown
---
number: 001
title: <タイトル>
status: todo            # todo | in-progress | done
priority: medium        # high | medium | low
labels: []              # 任意のラベル
created_at: 2026-06-14T00:00:00Z
updated_at: 2026-06-14T00:00:00Z
---

# <タイトル>

<本文（markdown 自由記述）>
```

`index.json` は `data-storage.md` の `.usagi/` 規約（`version` フィールド・atomic write・`to_string_pretty`）に従います。

```jsonc
{
  "version": 1,
  "issues": [
    {
      "number": 1,
      "title": "...",
      "status": "todo",
      "priority": "medium",
      "labels": [],
      "file": "001-xxx.md",
      "created_at": "2026-06-14T00:00:00Z",
      "updated_at": "2026-06-14T00:00:00Z"
    }
  ]
}
```

## やること

- `domain` に `Issue` / `IssueStatus` / `IssuePriority` などのエンティティを追加する（外部依存なし）。
- `infrastructure` に issue ストアを追加する。
  - `.usagi/issues/` 配下の `NNN-*.md` の読み書き（frontmatter のパース／シリアライズ）。
  - 連番 `number` の採番（既存ファイル / index の最大値 + 1。衝突しない採番）。
  - `index.json` の生成・更新（既存 store と共通の atomic read/write ヘルパーを利用）。
  - markdown 本体と index の整合性（フォールバック: index が無い／壊れている場合はファイル群から再構築）。
- `usecase` に issue 操作（`create` / `update` / `list` / `search` / `delete` / `get`）を追加する。
  - `search`: frontmatter（status / priority / labels）でのフィルタ + 本文・タイトルの全文一致。
- `usagi init` の `.gitignore` 追記処理（`usecase/project.rs::ignore_usagi_dir`）を、`.usagi/issues/` を共有しつつ他を無視する選択的パターンへ変更する。
  ```gitignore
  .usagi/*
  !.usagi/issues/
  ```
  既存の `.usagi/` 単独エントリとの後方互換・冪等性を保つ。
- `document/data-storage.md` に issue ストアの保存場所・ファイル形式・index 仕様を追記する。

## 完了条件

- `.usagi/issues/<NNN-feature>.md` として issue が読み書きできる。
- 採番が衝突せず連番で振られる。
- `index.json` が frontmatter と整合し、欠落／破損時はファイル群から再構築できる。
- `usagi init` 後の `.gitignore` で `.usagi/issues/` のみ git 追跡対象になり、`state.json` 等はローカルのままになる。
- カバレッジ 100% を維持する。
