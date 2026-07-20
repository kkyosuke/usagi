# 4. メモリ（`memory/`）

> [データ永続化トップ](README.md) ｜ ← 前へ [3. タスク issue（`issues/`）](03-issues.md)

AI エージェント（Claude Code など）が**セッションをまたいで覚えておくべき知識**を保存するフォーマットです。
issue がタスク（やること）を管理するのに対し、メモリは**ユーザーの好み・作業上の指針・プロジェクト固有の
前提・外部リソースへのポインタ**といった、コードや git 履歴からは読み取れない事実を蓄積します。
`<repo>/.usagi/` の他のファイルと異なり **git で共有**され、チームで同じ知識を参照できます。
`infrastructure/memory_store.rs` の `MemoryStore` が管理します。操作する CLI / MCP は
[3.1 CLI コマンド](../03-commands/01-cli.md#usagi-memory) / [3.3 MCP サーバ](../03-commands/03-mcp.md) を参照してください。

## 目次

- [保存場所](#保存場所)
- [メモリファイル（`<slug>.md`）](#メモリファイルslugmd)
- [`MEMORY.md`（目次）](#memorymd目次)
- [`index.json`（派生キャッシュ）](#indexjson派生キャッシュ)

## 保存場所

```
<repo>/.usagi/memory/
├── user-prefers-tabs.md   # 1 メモリ = 1 ファイル（名前 = ファイル名のステム）
├── deploy-steps.md
├── MEMORY.md              # 目次（1 メモリ = 1 行。git で共有）
├── index.json             # 派生キャッシュ（git 管理外）
└── .lock                  # プロセス間書き込みロック（git 管理外）
```

メモリファイル（`<slug>.md`）と目次 `MEMORY.md` は git 追跡対象、`index.json` は再生成可能なので
`.usagi/.gitignore` で除外します（[02-workspace.md#保存場所](02-workspace.md#保存場所)）。

`<repo>` は **操作したカレントの worktree のルート**です。issue と同じく、セッション worktree
（`.usagi/sessions/<name>/`。[04-orchestration.md](../04-orchestration.md)）内で保存したメモリは
そのセッション自身の `.usagi/memory/` に書かれ、セッションのブランチに乗って PR 経由で `main` に流れます
（ワークスペースのチェックアウトを未コミットで汚しません）。

MCP の root coordinator はメモリを読み取り・検索できますが、git 追跡対象を root branch 上で未コミット変更にしないため
`memory_save` / `memory_delete` は実行できません。更新は session worktree で行い、その branch の commit・PR に載せます。
root に既存の未コミットメモリがある場合の移行手順は
[MCP サーバの書き込みガードレール](../03-commands/03-mcp.md#root-に未コミットのメモリがある場合)を参照してください。

## メモリファイル（`<slug>.md`）

上部に **frontmatter**（行ベースのメタデータ）、その下に自由記述の markdown 本文を持ちます。ファイル名は
メモリの `name`（フィルナム安全なスラッグ）に `.md` を付けたもので、`name` がそのままメモリの識別子です。

```markdown
---
name: user-prefers-tabs
title: ユーザーはタブインデントを好む
type: user
related: [editor-config]
created_at: 2026-06-17T00:00:00+00:00
updated_at: 2026-06-17T00:00:00+00:00
---

本文（markdown 自由記述）。
```

| フィールド | 型 | 意味 |
|---|---|---|
| `name` | string | 一意な識別子（ファイル名のステムと一致）。保存時に与えた名前をスラッグ化して正規化する |
| `title` | string | 一行サマリ |
| `type` | enum | `user` / `feedback` / `project` / `reference`（既定 `project`） |
| `related` | array&lt;string&gt; | 関連するメモリの `name`（ブロックしない緩いリンク） |
| `created_at` / `updated_at` | RFC3339(UTC) | 作成・更新日時 |

`type` の意味は次のとおりです。

| type | 内容 |
|---|---|
| `user` | ユーザー自身（役割・専門性・好み） |
| `feedback` | 作業の進め方への指針（修正・確認済みの方針） |
| `project` | コード・git からは導けない進行中の作業・目標・制約 |
| `reference` | 外部リソースへのポインタ（URL・ダッシュボード・チケット） |

- frontmatter は `serde_yaml` 不採用の方針に合わせ、既知フィールドを対象にした軽量パーサで読み書きします。未知のキーは無視するので、フォーマットを後方互換に拡張できます。
- 書き込みはアトミック（一時ファイル + `rename`）。**`name` が一意な識別子**なので、同じ名前への保存は同じファイルを上書きし、重複が生まれません（`created_at` は保持されます）。
- 同一ストアに対する read-modify-write（既存メモリの読み取り→保存、`index.json` / `MEMORY.md` の更新）は、ストアごとの `.lock` ファイルに対するプロセス間排他ロック（advisory lock）で直列化します。MCP サーバと TUI が同じ `.usagi/memory/` を同時に書いても、保存と派生ファイル再構築が他プロセスと混ざらず、`created_at` の取り違えや目次の取りこぼしが起きません。

## `MEMORY.md`（目次）

セッション開始時にコンテキストへ読み込む 1 行サマリの目次です。`updated_at` の新しい順に、1 メモリ = 1 行で
リンクと種別を並べます。メモリ本体と同じく **git で共有**します（再生成可能ですが、エージェントが最初に読む
入口として追跡対象に含めます）。

```markdown
# Memory

- [ユーザーはタブインデントを好む](user-prefers-tabs.md) — user
- [デプロイ手順](deploy-steps.md) — project
```

## `index.json`（派生キャッシュ）

一覧・検索を速くするための、各メモリのメタデータ（本文を除く）のキャッシュです。`version` を持ち、markdown
ファイルが常に **正**。保存・削除では該当メモリのエントリだけを差し替える**増分更新**で（`MEMORY.md` はその
結果から再描画）、markdown 全件の読み直しは行いません。`index.json` が無い／壊れている場合のみ markdown 群から
自動再構築されるため、欠落しても整合性は損なわれません（だから git 管理外でよい）。再生成可能なキャッシュなので、
書き込みはアトミック（一時ファイル + `rename`）だが fsync は行いません（`MEMORY.md` は commit 対象のため durable に保存）。

```jsonc
{
  "version": 1,
  "memories": [
    {
      "name": "user-prefers-tabs",
      "title": "ユーザーはタブインデントを好む",
      "type": "user",
      "related": ["editor-config"],
      "file": "user-prefers-tabs.md",
      "created_at": "2026-06-17T00:00:00+00:00",
      "updated_at": "2026-06-17T00:00:00+00:00"
    }
  ]
}
```

| 層 | モジュール | 役割 |
|---|---|---|
| domain | `domain/memory/` | `Memory` / `MemorySummary` / `MemoryType`、frontmatter の読み書き・スラッグ化 |
| infrastructure | `infrastructure/memory_store.rs` | `.usagi/memory/` の走査・読み書き、`MEMORY.md` 目次と `index.json` の生成・再構築 |
| usecase | `usecase/memory/` | `save`（upsert） / `get` / `list` / `search` / `update` / `delete` と種別フィルタ・全文検索 |
