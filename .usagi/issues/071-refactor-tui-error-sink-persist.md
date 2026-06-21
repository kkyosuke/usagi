---
number: 71
title: refactor(tui): TUI のエラー記録を単一シンクへ集約しファイルにも永続化する
status: done
priority: medium
labels: [refactor, tui, error-log]
dependson: []
related: [70, 72, 73]
created_at: 2026-06-21T00:00:01.000000+00:00
updated_at: 2026-06-21T03:23:20.664099+00:00
---

## 背景

error-log PR #236 で、セッション作成・削除・起動の失敗を `ErrorLog::record` でファイルへ書き出すようにした。ただし記録の方針が presentation の複数箇所（`run_create` / `run_remove` / `open_terminal`）に**重複**しており、単一の「エラーシンク」が存在しない。

さらに、同じ「エラー発生」という事象に対し **2 系統の記録経路**が並存している:

- 画面内ログ: `HomeState::log_error`（`LogLine::error`、メモリ内のみ）
- ファイルログ: `ErrorLog::record`（一部の失敗のみ）

その結果「画面に出るエラー＝ファイルに残るエラー」になっておらず、`preview` 失敗・設定保存失敗・issue/memory 操作失敗など多くの TUI エラーはファイルに残らない。

## やること

- TUI のエラー記録を**単一のシンク**に集約し、画面表示とファイル永続化を 1 経路で扱う。クリーンアーキテクチャの依存方向（`presentation → infrastructure`）を保ったまま、次のいずれかで実装する:
  - **案 A（Logger 注入）**: infrastructure に `Logger` トレイト（`record(&str)`）を定義し、`HomeState` へ注入。`log_error` がメモリログとファイルの双方へ流す。テストは no-op を注入。
  - **案 B（Effect 境界）**: `HomeState` は「永続化すべきエラー」を Effect として返し、event-loop 境界で `ErrorLog::record` を呼ぶ。presentation が infrastructure を直接知らない。
- ノイズ抑制のため「記録対象のエラー（操作失敗）」と「単なるユーザー入力ミス（unknown command 等）」を区別できる設計にする。
- `run_create` / `run_remove` / `open_terminal` に散在する `ErrorLog::record(&format!(...))` をこのシンク経由へ寄せ、重複を解消する。

## 確認方法

- 画面に出る操作失敗（セッション操作・`preview`・設定保存など）が `~/.usagi/logs/` にも残ること。入力ミス系はノイズとして残さない方針が一貫すること。
- 注入したシンクのテスト（no-op / 記録の検証）が書けること。
- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
