---
number: 14
title: usagi clean（古いセッションの整理）
status: done
priority: low
labels: [cli]
dependson: [3]
related: []
created_at: 2026-06-16T23:01:26.545540+00:00
updated_at: 2026-06-25T00:00:00.000000+00:00
---

# `usagi clean`

## 概要

長期間放置された不要なセッション（worktree）や古い状態データを一括でクリーンアップするコマンドを実装します。ディスク容量の節約とプロジェクトのクリーンな状態維持を支援します。

## やること

- 最終更新から一定期間経過したセッション（worktree）を検索し、削除を提案する。
- 重複した一時ファイルや `.usagi/` 内の古い状態データを整理する。
- 削除前に対象を一覧表示し、確認（dry-run / 対話確認）してから実行する。

## 完了条件

- `usagi clean` で放置セッションが検出され、確認のうえ削除できる。
- `--dry-run` で削除対象だけを表示できる。

## 実装状況

実装済み（AI バックグラウンド実行版）。`usagi clean` は設定中の Agent CLI をヘッドレス・デタッチ起動し、
放置・マージ済みのセッション worktree を AI が完全自律で判断・削除する。`usagi clean` 自体は即座に制御を返す。

- 起動形態: Agent CLI を非対話モード（Claude `-p` / Codex `exec` / Gemini `-p`）でバックグラウンドにデタッチ起動。
  出力は `<workspace>/.usagi/clean.log` に追記。無人実行のため各 CLI の権限承認をバイパスするフラグを付与
  （Claude `--dangerously-skip-permissions` / Codex `--dangerously-bypass-approvals-and-sandbox` / Gemini `--yolo`）。
- 削除対象: `.usagi/sessions/<name>/` 配下のマージ済み・放置セッション worktree に限定。リポジトリ本体・`main/`・
  未コミット変更のある worktree には触れないようプロンプトで明示。
- 自律性: AI が判断して削除まで実行（事前確認なし）。`--dry-run` で削除させず対象の報告のみ。`--agent <NAME>` で
  既定 CLI を上書き可能。
- 設計: `Agent` トレイトに `headless_command` を追加し、claude / codex / gemini 各アダプタが実装。CLI 起動の
  オーケストレーションは `presentation/cli/clean.rs`。掃除タスクのプロンプト構築・agent 解決は純粋関数でテスト済み。

ドキュメント: [03-commands/01-cli.md](../../document/03-commands/01-cli.md#usagi-clean) / [01-overview.md](../../document/01-overview.md)。
