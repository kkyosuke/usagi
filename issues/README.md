# Issues

[usagi.ai](https://github.com/KKyosuke/usagi.ai) の設計・機能を本プロジェクトへ移植するための対応 issue 一覧です。
各 issue は `NNN-feature.md` 形式で、上部にメタデータ（`status` / `priority` / `dependson` など）、その下に概要を記述しています。

## 凡例

- **status**: `todo` / `in-progress` / `done`（一覧では完了 issue を ✅ で示す）
- **priority**: `high` / `medium` / `low`
- **dependson**: 先に完了している必要がある issue 番号

## 一覧

| # | feature | 概要 | category | priority | dependson |
|---|---|---|---|---|---|
| 001 | [init-cli](001-init-cli.md) ✅ | `usagi init <URL>` CLI コマンド | cli | high | — |
| 002 | [workspace-screen](002-workspace-screen.md) ✅ | ワークスペース画面とコマンドモード基盤 | tui | high | — |
| 003 | [session](003-session.md) ✅ | `session` セッション管理（`.usagi/worktree` 配下に再帰的に worktree 構築） | tui | high | 002 |
| 004 | [space](004-space.md) ✅ | `session switch` セッション切り替え | tui | high | 002, 003 |
| 005 | [ai](005-ai.md) | `ai` AI エージェントへの指示・対話 | tui | high | 002 |
| 006 | [terminal](006-terminal.md) ✅ | `terminal` 対話型ターミナル | tui | medium | 002, 003 |
| 007 | [history](007-history.md) | `history` コマンド履歴表示 | tui | medium | 002 |
| 008 | [man](008-man.md) ✅ | `man` ヘルプ表示 | tui | low | 002 |
| 009 | [sync](009-sync.md) | `usagi sync` main の変更を同期 | cli | medium | 003 |
| 010 | [finish](010-finish.md) | `usagi finish` セッション統合・削除 | cli | high | 003 |
| 011 | [list](011-list.md) | `usagi list` 全セッション俯瞰 | cli | medium | 003 |
| 012 | [diff](012-diff.md) | TUI Diff ビューア | tui | medium | 002, 003 |
| 013 | [logs](013-logs.md) | `usagi logs` 履歴の閲覧・検索 | cli | low | 007 |
| 014 | [clean](014-clean.md) | `usagi clean` 古いセッション整理 | cli | low | 003 |
| 015 | [config-edit](015-config-edit.md) ✅ | `usagi config --edit` 設定編集 | cli | medium | 001 |
| 016 | [context](016-context.md) | `usagi context` AI 用コンテキスト生成 | cli | low | 001 |
| 017 | [init-agent](017-init-agent.md) | `usagi init-agent` エージェント設定生成 | cli | low | 001 |
| 018 | [alias](018-alias.md) | `usagi alias` コマンドエイリアス | cli | low | 005 |
| 019 | [doctor-fix](019-doctor-fix.md) ✅ | `usagi doctor --fix` 依存自動修復 | cli | medium | — |
| 020 | [gh-issue](020-gh-issue.md) | gh Issue 連携セッション作成 | cli | low | 003 |
| 021 | [local-settings](021-local-settings.md) ✅ | プロジェクト単位のローカル設定（設定上書き） | core | medium | — |
| 022 | [local-settings-ui](022-local-settings-ui.md) ✅ | ローカル設定の編集 UI | tui | medium | 021 |
| 023 | [issue-store](023-issue-store.md) | issue ストア（`.usagi/issues/` への永続化と採番） | core | high | — |
| 024 | [issue-cli](024-issue-cli.md) | `usagi issue` サブコマンド（CRUD・検索） | cli | high | 023 |
| 025 | [issue-mcp](025-issue-mcp.md) | `usagi mcp` で issue 操作を LLM に公開 | mcp | high | 023, 024 |

## 依存関係

```mermaid
graph TD
    001[001 init-cli]
    002[002 workspace-screen]
    003[003 session]
    019[019 doctor-fix]

    002 --> 003
    002 --> 004[004 space]
    003 --> 004
    002 --> 005[005 ai]
    002 --> 006[006 terminal]
    002 --> 007[007 history]
    002 --> 008[008 man]
    003 --> 009[009 sync]
    003 --> 010[010 finish]
    003 --> 011[011 list]
    002 --> 012[012 diff]
    003 --> 012
    007 --> 013[013 logs]
    003 --> 014[014 clean]
    001 --> 015[015 config-edit]
    001 --> 016[016 context]
    001 --> 017[017 init-agent]
    005 --> 018[018 alias]
    003 --> 020[020 gh-issue]
    023[023 issue-store]
    023 --> 024[024 issue-cli]
    023 --> 025[025 issue-mcp]
    024 --> 025
```

## 推奨着手順

1. **基盤**: 001 init-cli / 002 workspace-screen / 019 doctor-fix（依存なし、並行可能）
2. **セッション中核**: 003 session → 004 space / 010 finish / 011 list
3. **作業支援**: 005 ai / 006 terminal / 007 history / 012 diff
4. **後続・拡張**: 009 sync / 013 logs / 014 clean / 015〜018 / 020
5. **issue 管理（タスク管理）**: 023 issue-store → 024 issue-cli → 025 issue-mcp

> [!NOTE]
> 本一覧は `usagi.ai` の `issue/` および `doc/` を参照して作成しています。実装済みの機能（`doctor` / `hop` / `status`、Welcome/Home/New/Open/Config 画面、通知）は対象外です。
