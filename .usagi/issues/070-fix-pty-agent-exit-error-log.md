---
number: 70
title: fix(pty): エージェント/シェルの異常終了（非ゼロ exit）をエラーログに記録する
status: done
priority: high
labels: [fix, infrastructure, error-log]
dependson: []
related: [71]
created_at: 2026-06-21T00:00:00.000000+00:00
updated_at: 2026-06-21T03:23:20.664099+00:00
---

## 背景

エラーログ（`~/.usagi/logs/`）には、セッションの**起動**失敗（`run_create` / `run_remove` / `open_terminal` の spawn 失敗）が記録されるようになった（error-log PR #236）。しかし「起動には成功したが、その中のエージェント/シェルが落ちた」ケースは依然として残らない。

`src/infrastructure/pty.rs` の reader スレッドは PTY 出力を読み、シェルが終了すると EOF でペインを閉じるだけで、**子プロセスの exit status を一切記録しない**。`-c` シェルはエージェント CLI が終了すると終了するため、エージェントが panic / 非ゼロ終了しても usagi 側にはペインが閉じた事実しか残らない。

→ ユーザーから見た「session c が失敗した」の多くはこの経路（起動後のランタイム失敗）に該当し得るが、現状では追跡できない。

## やること

- PTY の子プロセス（`-c` シェル、その先のエージェント CLI）の終了コードを取得し、**非ゼロ終了や signal 終了を `ErrorLog::record` で記録**する。
  - 記録例: `"agent session in <worktree> exited with status <code>"`。
- 正常終了（exit 0、ユーザーが意図的に閉じた）はログに残さない（ノイズ防止）。
- exit status の取得経路（`portable-pty` の `Child::wait` / `try_wait`）と、reader スレッド・`terminal_pane` のペイン終了処理（`PaneStep::Closed`）との責務境界を整理する。`pty.rs` はカバレッジ除外（`scripts/coverage.sh` の `COVERAGE_IGNORE`）対象なので、ロジックはテスト可能な層へ寄せられないか検討する。

## 確認方法

- 故意に失敗するコマンド（例: 存在しないエージェント CLI、即座に非ゼロ終了するスクリプト）でペインを開き、`~/.usagi/logs/error-YYYY-MM-DD.log` に終了ステータスが記録されること。
- 正常に閉じたセッションはログに残らないこと。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
