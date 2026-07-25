---
number: 549
title: fix(tui): open した path と daemon の workspace の不整合を閉じる
status: done
priority: medium
labels: [v2, tui, ipc, daemon, correctness]
dependson: []
related: [548, 542]
created_at: 2026-07-25T06:54:25.075274+00:00
updated_at: 2026-07-25T13:00:32.295084+00:00
---

## 問題・根拠

[#548](548-fix-ipc-handshake-client-workspace-root.md) の workspace fence は **client process の workspace context**（`USAGI_WORKSPACE_ROOT`、または canonical cwd）を daemon の trusted root と突き合わせる。これで「workspace B の cwd から実行した client が A の daemon に接続する」経路は閉じたが、**TUI が cwd とは別の path を明示的に開く経路**は残っている。

- `usagi open <path>` / `usagi <path>` は `EntryScreen::Workspace { path }` を作り、`path` の workspace 画面を描画する。
- 一方 session 一覧は daemon から読む（`request_lifecycle_snapshot`）。daemon は起動時に確定した 1 つの workspace しか serve しない。
- したがって cwd が daemon の trusted root の配下でありながら `<path>` がそれ以外の workspace を指す場合、handshake は admit され、**title は `<path>` の workspace なのに session 一覧は daemon の workspace のもの**という表示になる。`document/04-ipc.md` の [workspace fence](../../document/04-ipc.md#workspace-fence) に既知の不整合として記載した。
- `usagi hop` で Recent から別 workspace を選ぶ経路も同じ形になる（同一 process で複数 workspace を順に開く）。

## 設計上の判断が必要な点

fence を「選択した workspace path」で申告するだけでは、daemon が所有していない workspace を開く操作が常に拒否される（`usagi open <別 workspace>` が失敗する）。どちらを正とするか先に決める必要がある。

- **選択した workspace を申告して拒否する**: 一貫するが、複数 workspace を跨ぐ hop / open が daemon 停止なしには使えなくなる。拒否理由の提示と復帰手順（対象 workspace の daemon を起動する）を UI に用意する必要がある。
- **daemon を workspace ごとに持つ**: data directory は workspace 非依存で、`daemon.json` / `current.json` は 1 daemon 前提である。`bound_workspace_root` は durable `repository_root` を優先するため、同一 data directory で 2 つ目の workspace を bind できない。workspace ごとの locator / record を導入するか、data directory を workspace scope に分けるかの選択になる。
- **TUI 側で daemon の workspace 以外を開かせない**: 実装は軽いが、workspace 切り替えという TUI の役割を制限する。

## 受入条件

- `usagi open <path>` / `hop` の選択が daemon の workspace と異なる場合の挙動が決まり、テストで固定される（別 workspace の session 一覧を別 workspace の title で表示しない）。
- 決定した挙動を `document/03-tui.md` と `document/04-ipc.md#workspace-fence` に反映し、#548 が記録した既知の不整合の記述を更新する。
- カバレッジ 100% を維持する。
