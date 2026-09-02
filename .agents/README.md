# .agents

AI エージェント（Claude Code / Codex / Gemini CLI など）固有の作業手順と、完了済み issue に紐づく
historical design を置くディレクトリ。
ルートの `CLAUDE.md` / `AGENTS.md` / `GEMINI.md` からここを読み込む（3 ファイルは各 CLI の入口で、内容は同一）。

| ファイル | 内容 |
|---|---|
| [workflow.md](./workflow.md) | 開発ワークフロー（新規作業 / 追加修正の手順） |
| [designs/258-controller-runtime-migration.md](./designs/258-controller-runtime-migration.md) | issue #258 の controller runtime 移行設計（履歴） |
| [designs/372-modal-component-refactor.md](./designs/372-modal-component-refactor.md) | issue #372–#374 の modal component 設計（履歴） |

## Documentation

| | 置き場所 | 読み手 | 内容 |
|---|---|---|---|
| **Project specifications and conventions** | `document/` | Developers + AI | Architecture, conventions, and other human-readable project documentation (index: [document/README.md](../document/README.md)) |
| **タスク（issue）** | `.usagi/issues/` | 開発者 + AI | 実装すべき機能を `NNN-feature.md` 形式で管理する issue ストア。MCP の `issue_*` ツールで操作する。新規作業はここから着手する issue を選ぶ。 |
| **エージェント作業手順** | `.agents/workflow.md` | AI エージェント | worktree 運用や PR までの進め方など、AI に守らせたいオペレーション |
| **完了済み設計履歴** | `.agents/designs/` | 開発者 + AI | issue 実装時の設計記録。現在仕様は所有せず、冒頭の Baseline が指す `document/` を参照する |

- 規約（アーキテクチャ・ブランチ名・コミット・PR・品質チェック）は開発者も従うため `document/06-conventions.md` に置く。
- `workflow.md` はそれらの規約を前提に「どう作業を進めるか」を扱う。`designs/` は完了時点の判断を保存する履歴であり、
  現在のアーキテクチャや挙動は所有しない。
