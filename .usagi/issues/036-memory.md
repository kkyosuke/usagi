---
number: 36
title: メモリ機能（.usagi/memory/ への永続化と CLI / MCP 公開）
status: todo
priority: medium
labels: [core, cli, mcp]
dependson: [23]
related: [25]
created_at: 2026-06-17T13:16:32.000000+00:00
updated_at: 2026-06-17T13:16:32.000000+00:00
---

# メモリ機能（AI エージェントの永続知識ストア）

## 概要

AI エージェント（Claude Code 等）が**セッションをまたいで覚えておくべき知識**を「メモリ」として永続化する機能を追加します。issue がタスク（やること）を管理するのに対し、メモリは**ユーザーの好み・作業上の指針・プロジェクト固有の前提・外部リソースへのポインタ**といった、コードや git 履歴からは読み取れない事実を蓄積します。

保存先は `<repo>/.usagi/memory/`。1 メモリ = 1 ファイルの **frontmatter 付き markdown** とし、[023-issue-store](023-issue-store.md) の永続化基盤（atomic write・index 再構築・選択的 `.gitignore`）と同じ規約に従います。メモリはチームで共有したい知識のため、issue と同様に **git 管理対象**に含めます。

issue ストア（023〜025）のパターンを踏襲し、本 issue でドメイン・永続化・usecase に加えて CLI / MCP の公開までを一括で扱います（規模が大きくなる場合は store / CLI / MCP に分割可）。

## メモリのデータ構造

1 メモリ = 1 事実を 1 ファイルに保存します。ファイル名は `<slug>.md`。

```markdown
---
name: <short-kebab-case-slug>
title: <一行サマリ>
type: user            # user | feedback | project | reference
created_at: 2026-06-17T00:00:00Z
updated_at: 2026-06-17T00:00:00Z
related: []           # 関連メモリの name 一覧
---

<事実本文。feedback / project は **Why:** と **How to apply:** を続ける。
関連メモリは [[name]] でリンクする。>
```

`type` の意味:

| type | 内容 |
|---|---|
| `user` | ユーザー自身（役割・専門性・好み） |
| `feedback` | 作業の進め方への指針（修正・確認済みの方針、理由を含める） |
| `project` | コード・git からは導けない進行中の作業・目標・制約（相対日付は絶対日付へ変換） |
| `reference` | 外部リソースへのポインタ（URL・ダッシュボード・チケット） |

### インデックス（MEMORY.md）

セッション開始時にコンテキストへ読み込む 1 行サマリの目次を `.usagi/memory/MEMORY.md` に置きます。1 メモリ = 1 行（`- [Title](file.md) — フック`）で、本文は書きません。検索高速化用の `index.json`（`version` / `to_string_pretty` / atomic write）も issue ストアと同様に併設します。

## やること

- `domain` に `Memory` / `MemoryType` エンティティを追加する（外部依存なし）。
- `infrastructure` にメモリストアを追加する。
  - `.usagi/memory/<slug>.md` の読み書き（frontmatter のパース／シリアライズ）。
  - `MEMORY.md` 目次と `index.json` の生成・更新（issue ストアと共通の atomic read/write ヘルパーを再利用）。
  - 欠落／破損時はファイル群から目次・index を再構築する。
- `usecase` にメモリ操作（`save` / `update` / `list` / `search` / `delete` / `get` / `recall`）を追加する。
  - `save`: 既存の同一トピックがあれば新規作成せず更新する（重複防止）。
  - `recall`: クエリ・type で関連メモリを抽出（セッション開始時のコンテキスト注入を想定）。
- `presentation/cli` に `usagi memory` サブコマンドを追加する（issue CLI と同じ薄い層）。

  | コマンド | 説明 |
  |---|---|
  | `usagi memory save` | メモリを保存（`--name` / `--type` / `--title` / `--body`、本文 `-` で stdin） |
  | `usagi memory list` | 一覧（`--type` でフィルタ、既定は `updated_at` 降順） |
  | `usagi memory show <name>` | 指定メモリの frontmatter + 本文を表示 |
  | `usagi memory update <name>` | type / title / 本文の更新 |
  | `usagi memory search <query>` | タイトル・本文の全文検索 |
  | `usagi memory delete <name>` | 削除（`--yes` なしは確認） |

  - `--json` で機械可読出力に切り替え可能にする。
- `presentation` の MCP サーバ（[025-issue-mcp](025-issue-mcp.md)）に `memory_save` / `memory_update` / `memory_list` / `memory_search` / `memory_delete` / `memory_get` ツールを追加し、同じ usecase を再利用する。LLM が事実を保存・想起できる説明を付与する。
- `usagi init` の選択的 `.gitignore`（`.usagi/*` ＋ `!.usagi/issues/`）に `!.usagi/memory/` を追加し、メモリを git 共有対象にする（冪等性・後方互換を保つ）。
- ドキュメントを更新する。
  - `document/data/` にメモリストアの保存場所・ファイル形式・`MEMORY.md` / `index.json` 仕様を追記。
  - `document/03-commands/` に `usagi memory` を追記。
  - `document/05-settings.md` に関連設定があれば追記。
  - `README.md` に AI エージェントからの利用方法（MCP 登録・想起の流れ）を追記。

## 完了条件

- `.usagi/memory/<slug>.md` としてメモリが読み書きでき、`MEMORY.md` 目次と `index.json` が本文と整合する。
- 目次・index の欠落／破損時にファイル群から再構築できる。
- `usagi memory` の各サブコマンドでメモリの保存・一覧・表示・更新・検索・削除ができ、`--json` 出力が得られる。
- MCP クライアント（Claude Code 等）から `memory_*` ツールでメモリを操作できる。
- `usagi init` 後の `.gitignore` で `.usagi/memory/` が git 追跡対象になり、`state.json` 等はローカルのままになる。
- カバレッジ 100% を維持する。

> 依存: 本 issue は [023-issue-store](023-issue-store.md)（永続化基盤）を前提とし、MCP 公開は [025-issue-mcp](025-issue-mcp.md) のサーバ実装を拡張します。
