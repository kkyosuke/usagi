# 3. タスク issue（`issues/`）

> [データ永続化トップ](README.md) ｜ ← 前へ [2. workspace 毎（リポジトリ単位）](02-workspace.md) ｜ 次へ → [4. メモリ（`memory/`）](04-memory.md)

プロジェクトのタスクを管理する issue の保存フォーマットです。`<repo>/.usagi/` の他のファイルと異なり
**git で共有**され、チームで同じタスク一覧を見られます。`infrastructure/issue_store.rs` の `IssueStore` が
管理します。操作する CLI / MCP は [3.1 CLI コマンド](../03-commands/01-cli.md#usagi-issue) /
[3.3 MCP サーバ](../03-commands/03-mcp.md) を参照してください。

## 目次

- [保存場所](#保存場所)
- [issue ファイル（`NNN-<slug>.md`）](#issue-ファイルnnn-slugmd)
- [`index.json`（派生キャッシュ）](#indexjson派生キャッシュ)
- [依存関係の解決（着手可能な issue）](#依存関係の解決着手可能な-issue)

## 保存場所

```
<repo>/.usagi/issues/
├── 001-add-doctor-command.md   # 1 issue = 1 ファイル
├── 002-fix-login.md
└── index.json                  # 派生キャッシュ（git 管理外）
```

issue ファイル（`NNN-*.md`）は git 追跡対象、`index.json` は再生成可能なので `.usagi/.gitignore` で除外します
（[02-workspace.md#保存場所](02-workspace.md#保存場所)）。

`<repo>` は**ワークスペースルート**（`.usagi/sessions/` の親）です。CLI・MCP・TUI のいずれも、セッション内
（`.usagi/sessions/<名>/…`）から実行された場合でもこのルートに解決してから読み書きするため、**全セッションが
同じ 1 つの issue ストアを共有**します（更新は常にルートの 1 か所へ書かれ、別セッションからも即座に見えます）。

各セッションは git worktree なので、追跡対象の `.usagi/issues/*.md` を自分の作業ツリーにもチェックアウトしています。
これがルートと食い違ったまま残ると（古いセッションブランチを後でマージした際に）ルートの issue を巻き戻しかねません。
そこで issue を作成・更新・削除するたびに、ルートの markdown ファイルを**各セッションの `.usagi/issues/` へミラー**して
コピーを一致させます（`index.json` キャッシュはセッション側で再生成されるため対象外）。ミラーはベストエフォートで、
ルートへの書き込みが正本です（あるセッションの更新に失敗しても issue 操作自体は成功し、次回の変更で再同期されます）。
すでに issue ストアを持つセッションだけが対象で、持たないセッションに新規作成することはありません。

## issue ファイル（`NNN-<slug>.md`）

上部に **frontmatter**（行ベースのメタデータ）、その下に自由記述の markdown 本文を持ちます。ファイル名は
`番号(3桁ゼロ詰め)-タイトルのスラッグ.md`。

```markdown
---
number: 1
title: doctor コマンドを追加
status: todo
priority: medium
labels: [cli, infra]
dependson: [2, 3]
related: [5]
parent: 4
milestone: v1
created_at: 2026-06-14T00:00:00+00:00
updated_at: 2026-06-14T00:00:00+00:00
---

# doctor コマンドを追加

本文（markdown 自由記述）。
```

| フィールド | 型 | 意味 |
|---|---|---|
| `number` | u32 | 採番された一意な番号（ファイル名の接頭辞と一致）。新規作成時に「既存の最大値 + 1」で振る |
| `title` | string | タイトル |
| `status` | enum | `todo` / `in-progress` / `done` |
| `priority` | enum | `high` / `medium` / `low`（既定 `medium`） |
| `labels` | array&lt;string&gt; | 任意のラベル |
| `dependson` | array&lt;u32&gt; | 先に `done` になっている必要がある issue 番号（ブロックする先行条件） |
| `related` | array&lt;u32&gt; | 関連する issue 番号（ブロックしない緩いリンク） |
| `parent` | u32? | 親 issue 番号（Epic ⊃ サブタスクの階層）。`dependson`（先行条件）とは別概念 |
| `milestone` | string? | 束ねるマイルストーン名 |
| `created_at` / `updated_at` | RFC3339(UTC) | 作成・更新日時 |

- `parent` / `milestone` は値があるときだけ frontmatter に出力します（未設定の issue には行自体が現れません）。`labels` / `dependson` / `related` は空でも `[]` を書きます。
- frontmatter は `serde_yaml` 不採用の方針に合わせ、既知フィールドを対象にした軽量パーサで読み書きします。未知のキーは無視するので、フォーマットを後方互換に拡張できます。
- 書き込みはアトミック（一時ファイル + `rename`）。タイトル変更でスラッグが変わった場合は、同じ番号の旧ファイルを削除して 1 issue = 1 ファイルを保ちます。

## `index.json`（派生キャッシュ）

一覧・検索を速くするための、各 issue のメタデータ（本文を除く）のキャッシュです。`version` を持ち、markdown ファイルが常に **正**。`index.json` が無い／壊れている場合は markdown 群から自動再構築されるため、欠落しても整合性は損なわれません（だから git 管理外でよい）。

```jsonc
{
  "version": 1,
  "issues": [
    {
      "number": 1,
      "title": "doctor コマンドを追加",
      "status": "todo",
      "priority": "medium",
      "labels": ["cli"],
      "dependson": [2, 3],
      "related": [5],
      "parent": 4,
      "milestone": "v1",
      "file": "001-add-doctor-command.md",
      "created_at": "2026-06-14T00:00:00+00:00",
      "updated_at": "2026-06-14T00:00:00+00:00"
    }
  ]
}
```

## 依存関係の解決（着手可能な issue）

一覧・検索では各 issue に **着手可能か（ready）** を付与します。`dependson` に挙げた issue が **すべて `done`** で、かつ自身が `done` でないものを ready とみなします（存在しない依存番号は未達として扱う）。これにより「今すぐ着手できるタスク」を絞り込めます。

| 層 | モジュール | 役割 |
|---|---|---|
| domain | `domain/issue.rs` | `Issue` / `IssueSummary` / `IssueStatus` / `IssuePriority`、frontmatter の読み書き |
| infrastructure | `infrastructure/issue_store.rs` | `.usagi/issues/` の走査・読み書き、`index.json` の生成・再構築・採番 |
| usecase | `usecase/issue.rs` | `create` / `get` / `list` / `search` / `update` / `delete` と ready 判定、進捗集計（`IssueStats`）・グルーピング（`group`）・依存ツリー（`dependency_tree`）、変更時の各セッションへのミラー（`mirror`） |
