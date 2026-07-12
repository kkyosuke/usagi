# .agents

AI エージェント（Claude Code / Codex / Gemini CLI など）**固有の作業手順**をまとめたディレクトリ。
ルートの `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` からここを読み込む（3 ファイルは各 CLI の入口で、内容は同一）。

| ファイル | 内容 |
|---|---|
| [workflow.md](./workflow.md) | 開発ワークフロー（新規作業 / 追加修正の手順） |

## `v1/document/` との使い分け

| | 置き場所 | 読み手 | 内容 |
|---|---|---|---|
| **プロジェクト仕様・規約** | `v1/document/` | 開発者 + AI | 概要・画面設計・データ構造・[開発規約](../v1/document/06-conventions.md) など、人間も読むべき情報（目次は [v1/document/README.md](../v1/document/README.md)） |
| **タスク（issue）** | `.usagi/issues/` | 開発者 + AI | 実装すべき機能を `NNN-feature.md` 形式で管理する issue ストア。`usagi issue` コマンド / MCP ツールで操作する。新規作業はここから着手する issue を選ぶ。 |
| **エージェント作業手順** | `.agents/` | AI エージェント | worktree 運用や PR までの進め方など、AI に守らせたいオペレーション |

- 規約（アーキテクチャ・ブランチ名・コミット・PR・品質チェック）は開発者も従うため `v1/document/06-conventions.md` に置く。
- `.agents/` はそれらの規約を前提に「どう作業を進めるか」だけを扱う。
