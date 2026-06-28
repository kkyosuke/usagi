---
number: 77
title: feat(tui): 統合(unite)モード — 複数ワークスペースを1画面にグループ表示する
status: todo
priority: medium
labels: [feat, tui]
dependson: []
related: []
created_at: 2026-06-28T00:07:50.655520+00:00
updated_at: 2026-06-28T00:07:50.655520+00:00
---

## 目的

複数のワークスペースのセッションを 1 つのホーム画面にまとめて表示・操作できる「統合(unite)モード」を追加する。左ペインをワークスペースのグループの積み重ねにする。

```
workspace1 ──────────
  ⌂ root
  ◆ session-1
  ● session-2
workspace2 ──────────
  ⌂ root
  ○ session-1
```

## 確定した設計判断

- **導線（どのプロジェクトを統合するか）**: Open 画面を**複数選択化**（Space トグル → Enter）。1 件なら従来どおり単一ホーム、2 件以上で統合ホーム。**直近の統合セットを記憶**して再現。動的追加/削除は将来コマンド（`unite add/remove`）。
- **新規セッション `c` の対象**: **カーソルがいるグループのワークスペース**。`session create/remove`・`issue`・`config` などワークスペーススコープのコマンドも同様にカーソルグループ基準。Session スコープ（`terminal`/`agent`/`close`）は対象セッションのパスから所属が自明。
- **実装方針**: 単一ワークスペースのホームを「グループ数 1 の特殊形」として一般化する。`groups.len() > 1` のときだけグループヘッダを描く。

## 横断で効いている前提

- **パスキーで横断安全**: ターミナルプール・`agent-state/`・`open-panes/`・`pr-links/` は worktree 絶対パスをキーにしており、複数 WS 混在でも衝突しない。
- **名前キーで要修飾**: アクティブ行・`previous_active`(Ctrl-^)・`select_by_name`・`resume-focus` はセッション名（文字列）でキーしている。統合では同名セッションが別 WS に並びうるため `(workspace, name)` または絶対パスで修飾が必要。
- `event::Wiring.workspace_root` が単一パス。統合では行のグループから解決する必要がある。

## フェーズ（子 issue）

1. 基盤リファクタ: `WorktreeList`/`HomeState` をグループ対応データモデルに一般化（本番は単一、複数は単体テストで実証）
2. Open 画面の複数選択＋直近セット記憶
3. 統合レンダリング（グループヘッダ・カーソルのヘッダ飛ばし・横断合計）
4. スコープ解決（`c`/`r`/`close`・コマンドパレットをカーソルグループ基準に）
5. 動的追加/削除コマンド `unite add/remove`
6. ドキュメント更新（`design/home/`・`02-open.md`・`04-orchestration.md`・`data/01-global.md`・必要なら README）

各フェーズで `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`、カバレッジ 100% を維持。
