---
number: 160
title: feat(daemon): daemon ライフサイクル制御プレーン（Step 1 スケルトン）
status: done
priority: medium
labels: [daemon, cli]
dependson: []
related: []
parent: 159
created_at: 2026-07-09T23:32:27.695034+00:00
updated_at: 2026-07-09T23:32:34.004173+00:00
---

Epic #159 の Step 1。daemon 化の土台となる**制御プレーン**を実装する（PTY 所有はまだ移さない）。

## 実装内容

- `usagi daemon <status|start|stop>`（`serve` は隠しサブコマンド）。トップレベルは `--help` から隠す（ユーザー可視の挙動がまだ無いため）。
- `<data-dir>/daemon/` にファイルベースのレコード（`daemon.json`＝pid）と stop マーカー（`stop`）を置き、複数プロセス間で協調（usagi の共有メモリ非依存の流儀を踏襲）。
- 単一インスタンス保証: `StoreLock` 下で `register` が生存 daemon の重複起動を拒否。stale レコード（プロセス消滅）は自動で引き取り。
- `stop` は生存 daemon に stop マーカーを立て、daemon の poll ループが検知して deregister して終了。stale レコードは掃除。

## 層構成（クリーンアーキ）

- `domain/daemon.rs` — `classify(pid, alive) -> DaemonState`（純粋）。
- `infrastructure/daemon_store.rs` — レコード / stop マーカーの read/write/clear。
- `usecase/daemon.rs` — start/stop/status/register/deregister（`alive`/`spawn` を注入）。
- `presentation/cli/daemon.rs` — サブコマンド parse・出力・serve ディスパッチ。
- `src/main.rs`（合成ルート・カバレッジ除外）— 実 `serve` ループ（poll＋sleep）と detached spawn。

## テスト

- domain/usecase/infrastructure/presentation の全分岐をユニットテストでカバー（カバレッジ 100%: lines/functions）。
- 実 serve ループ・spawn は合成ルートに隔離（`main.rs`）し、実機で start→status→stop と stale 掃除を確認。

## 設計

[document/proposals/02-daemon.md](../../document/proposals/02-daemon.md)
