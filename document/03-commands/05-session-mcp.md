# 3.5 セッション MCP サーバ（`usagi session-mcp`）

> [コマンドリファレンス](README.md) ｜ ← 前へ [3.4 ローカル LLM MCP サーバ](04-llm-mcp.md)

`usagi session-mcp` は、usagi のセッション（[4. オーケストレーション](../04-orchestration.md)）操作を
**MCP（Model Context Protocol）サーバ**として AI エージェントに公開するコマンドです。エージェント
（Claude Code など）が tool 呼び出しで**セッションを作成**し、**特定のセッションのエージェントに
プロンプトを送って**作業を委譲できます。コーディネータ役のエージェントが、並行する worktree に
タスクを振り分けるオーケストレータとして振る舞えるようになります。

## 目次

- [概要](#概要)
- [起動と登録](#起動と登録)
- [対応 tool 一覧](#対応-tool-一覧)
- [`session_prompt` の挙動](#session_prompt-の挙動)
- [アーキテクチャ](#アーキテクチャ)
- [設計上の選択](#設計上の選択)

## 概要

- **トランスポート**: stdio 上の **JSON-RPC 2.0**（[issue MCP サーバ](03-mcp.md) と同じ実装）。
- **対象ワークスペース**: 起動ディレクトリから遡って解決した**ワークスペースルート**
  （`.usagi/sessions/` と `state.json` を持つ階層）。セッション専用 worktree
  （`<root>/.usagi/sessions/<name>/...`）の中で起動された場合は、その `.usagi/sessions` の手前の階層が
  ルートになります。該当しない場合は起動ディレクトリ自身をルートとして扱います。
- **ロジックの共有**: `session_create` / `session_list` は CLI・TUI と同じ
  [`usecase/session`](../02-architecture.md#各層の責務) を呼ぶ薄いアダプタで、挙動（worktree 生成・
  `state.json` 記録・reconcile）は完全に一致します。

## 起動と登録

エージェント起動コマンドに issue サーバと並んで自動で wire されるため、通常は個別登録は不要です。
手元での確認はシェルから直接起動できます。

```bash
usagi session-mcp   # stdin から JSON-RPC を読み、stdout へ応答を書く
```

Claude Code 用の `--mcp-config` には次が含まれます（issue サーバと同居）。

```json
{
  "mcpServers": {
    "usagi":         { "command": "usagi", "args": ["mcp"] },
    "usagi-session": { "command": "usagi", "args": ["session-mcp"] }
  }
}
```

## 対応 tool 一覧

`tools/list` で以下の 3 tool を公開します。結果はいずれも JSON テキストで返ります。

| tool | 必須引数 | 任意引数 | 返り値 |
|---|---|---|---|
| `session_create` | `name` | — | 作成されたセッション（`name` / `root` / `worktrees`） |
| `session_list` | — | — | セッション配列（各要素に `name` / `root` / `created_at` / `worktrees`） |
| `session_prompt` | `name` / `prompt` | — | 対象セッションのエージェントの応答テキスト |

- `session_create` は `name` をセッション名（=全リポジトリで作成する新規ブランチ名）として、
  `<root>/.usagi/sessions/<name>/` に worktree を生成します。名前は空・パス区切り文字を含むものを拒否し、
  既存のセッション名は重複エラーになります（CLI と同じ検証）。
- `session_list` は `state.json` を読むだけの軽量クエリで、on-disk の reconcile は行いません。

入力スキーマ（JSON Schema）は `tools/list` のレスポンスに各 tool の `inputSchema` として含まれます。

## `session_prompt` の挙動

`session_prompt` は、対象セッションの **worktree をカレントディレクトリにして、設定された
エージェント CLI をヘッドレス（print）モードで起動**し（`<agent> -p <prompt>`）、その標準出力を
応答として返します。

- 起動するエージェント CLI は[設定](../05-settings.md)の `agent_cli`（プロジェクトローカル → グローバルの
  順に解決、既定は Claude）に従います。
- ヘッドレス起動した子プロセスには**いかなる MCP サーバも wire しません**。委譲先のセッションが
  さらにセッションを再帰生成することはありません。
- 作業はセッションのブランチ（worktree）上で隔離されます。TUI で同じセッションに対話的エージェントを
  開いている場合でも、`session_prompt` はファイルシステム（worktree）を共有する**別プロセス**として動きます。

## アーキテクチャ

```
コーディネータ Agent ⇄ (stdio JSON-RPC)
        │
        ▼
presentation/cli/session_mcp.rs … stdin ループ + エージェント CLI バックエンド（テスト不能・カバレッジ対象外）
        │  handle_line(line) ごとに委譲
        ▼
presentation/mcp/session.rs     … SessionMcpServer：tool 実装（JSON-RPC フレーミングは mcp/mod.rs と共有・100% テスト）
        │  create / list は usecase へ、prompt は AgentBackend 経由
        ▼
usecase/session                 … create / list / workspace_root_for（reconcile・state.json 記録）
（テスト時）FakeBackend / （本番）CliAgentBackend → `<agent> -p <prompt>`
```

| モジュール | 役割 |
|---|---|
| `presentation/cli/session_mcp.rs` | `usagi session-mcp` のエントリ。ワークスペースルート解決・stdin ループ・エージェント CLI へのシェルアウト。`mcp` 同様カバレッジ対象外。 |
| `presentation/mcp/session.rs` | `SessionMcpServer`。`McpService` を実装しセッション tool を提供（JSON-RPC フレーミングは `mcp/mod.rs` と共有）。`session_prompt` のエージェント起動を `AgentBackend` トレイトで抽象化。ユニットテストで網羅。 |
| `usecase/session` | tool が呼ぶビジネスロジック（`create` / `list` / `workspace_root_for`）。MCP 固有の知識は持たない。 |

依存方向はクリーンアーキテクチャに従い `presentation → usecase`。MCP 層は presentation に閉じています
（[2. アーキテクチャ](../02-architecture.md) 参照）。

## 設計上の選択

- **issue MCP と同じ最小実装**: `serde_json` のみで同期的に JSON-RPC を処理し、テスト不能な
  stdin ループ・シェルアウトだけをカバレッジ対象外にしています（[03-mcp.md](03-mcp.md) と同方針）。
- **ワークスペースルートはパスから解決**: セッション専用 worktree の中で起動される前提に合わせ、
  起動ディレクトリの `.usagi/sessions` セグメントからルートを復元します。引数や環境変数に依存しないため
  起動コマンドは固定（引数なし）で済みます。
- **委譲はヘッドレス起動**: 走行中の対話的エージェント（PTY）に外部プロセスから割り込む仕組みは持たず、
  worktree を共有する独立したヘッドレス起動として委譲します。MCP を wire しないことで再帰生成も防ぎます。
- **状態を持たない**: サーバはセッション状態を保持せず、各 tool 呼び出しが `state.json` を直接
  読み書きします。CLI・TUI と混在して使っても整合します。
