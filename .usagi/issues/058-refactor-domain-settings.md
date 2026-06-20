---
number: 58
title: refactor(domain): settings からエージェント起動コマンド生成ロジックを外層へ退避
status: todo
priority: medium
labels: [refactor, core, review]
dependson: []
related: []
created_at: 2026-06-19T22:16:22.422682+00:00
updated_at: 2026-06-19T22:16:22.422682+00:00
---

## 背景

`src/domain/settings.rs:56-240` に、外部 CLI 呼び出しの都合を握るプレゼンテーション/インフラ寄りのロジックが domain に混入している。

- `launch_command` / `mcp_config_json` / `claude_hooks_settings` / `json_escape` が、Claude CLI のフラグ仕様・MCP config の JSON 文字列・shell クォート規約を組み立てている。
- `json_escape` を手書きしているのは「domain を serde_json 非依存に保つため」とコメントにあるが、これは **domain がやるべきでない仕事をしているから依存を避ける羽目になっている兆候**。
- プロンプト文言定数 `SESSION_WORKTREE_PROMPT` / `LOCAL_LLM_PROMPT`（`:103-111`）も同根（エージェントへの指示文＝アプリ挙動仕様であり domain エンティティの責務でない）。

## 対応方針

- domain には `AgentCli` enum と方針データ（`AgentWiring`）だけ残す。
- コマンド文字列生成・MCP config・フック設定・プロンプト定数は `infrastructure/agent/`（エージェントアダプタ）か presentation へ移す。serde_json を使える層へ移れば `json_escape` 手書きも不要になる。
- 移動後、`domain/settings.rs` の実装は ~377 行 → ~200 行に縮む見込み。

## 確認方法

- domain が serde_json/shell-words に依存しなくなる（純データ層化）。
- エージェント起動コマンドが従来と同一に生成されること（既存テスト維持）。
- カバレッジ 100% 維持。
