# .agents

AI エージェント（Claude Code / Gemini CLI など）**固有の作業手順**をまとめたディレクトリ。
ルートの `CLAUDE.md` / `GEMINI.md` からここを読み込む。

| ファイル | 内容 |
|---|---|
| [workflow.md](./workflow.md) | 開発ワークフロー（新規作業 / 追加修正の手順） |

## `document/` との使い分け

| | 置き場所 | 読み手 | 内容 |
|---|---|---|---|
| **プロジェクト仕様・規約** | `document/` | 開発者 + AI | 概要・画面設計・データ構造・[開発規約](../document/conventions.md) など、人間も読むべき情報 |
| **エージェント作業手順** | `.agents/` | AI エージェント | worktree 運用や PR までの進め方など、AI に守らせたいオペレーション |

- 規約（アーキテクチャ・ブランチ名・コミット・PR・品質チェック）は開発者も従うため `document/conventions.md` に置く。
- `.agents/` はそれらの規約を前提に「どう作業を進めるか」だけを扱う。
