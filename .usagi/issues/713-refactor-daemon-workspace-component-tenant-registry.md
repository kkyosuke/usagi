---
number: 713
title: refactor(daemon): workspace に束縛された component を tenant registry 経由で解決する
status: todo
priority: high
labels: [v2, daemon, refactor, workspace]
dependson: [709]
related: [710]
created_at: 2026-08-21T02:02:40.666621+00:00
updated_at: 2026-08-21T02:02:40.666621+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。段階 3（#710）の前提として、実装中に分割した issue。

## 問題・根拠

#709 で tenant registry を入れたが、daemon 内の component は依然として **起動した workspace の `SessionRuntime` を構築時に捕まえている**。この状態で handshake だけ multi-tenant にすると、workspace B の client に A の session 一覧・A の設定 env・A の role catalog を返してしまう。

構築時に 1 つの runtime を握っているのは次の 6 か所である。

| component | 何に使っているか |
|---|---|
| Agent scope resolver / terminal scope resolver | scope 解決（request は既に `workspace_id` を持つ） |
| Agent provisioner（Codex / Claude） | working directory・workspace root・role catalog・設定 env |
| terminal profile（`TrustedLoginShell`） | 設定 env を引く workspace root |
| session teardown worker | 未完了 teardown の journal |
| PR refresh worker | PR inventory から落としてよい session の判定 |
| startup の Agent / delegation 再照合 | 生存 session の集合 |

## 方針

- `WorkspaceRuntimes` port（`workspace(id)` / `workspace_at(root)` / `all()`）を registry に実装し、上記の component はこれを持つ。
- request が `workspace_id` を持つもの（scope 解決・Agent provision・terminal profile）は **その identity で引く**。
- daemon 全体で 1 つしかない集約（PR inventory の prune、Agent の session 再照合）は **全 tenant の和** を使う。1 workspace 分で判定すると他 workspace の記録を消す。
- teardown journal は全 tenant の未完了を集め、完了報告は `PendingTeardown.repository_root` で元の workspace へ routing する。
- connection は「束縛された workspace（session command の宛先）」と「registry（request が名指す workspace の解決）」の組を持つ（`ConnectionWorkspace`）。

tenant は 1 つのままなので **挙動は変わらない**。handshake の tenant 解決は #710 で行う。

## 受入条件

- 上記 6 か所が registry 経由になり、構築時に単一 runtime を捕まえる経路が product code に残らない。
- 既存の unit / 結合 / E2E テストが挙動不変で通る。
- カバレッジ 100% を維持する。
