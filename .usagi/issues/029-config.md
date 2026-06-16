---
number: 29
title: config コマンド（ホーム画面から設定画面を開く）
status: done
priority: medium
labels: [tui]
dependson: [2, 22]
related: []
created_at: 2026-06-16T23:05:23.808153+00:00
updated_at: 2026-06-16T23:08:30.114710+00:00
---

# `config` コマンド（ホーム画面から設定画面を開く）

## 概要

これまで Config 画面（設定編集）は Welcome（起動）画面からしか開けませんでした。ワークスペースで作業中に設定を変えたい場合、一度ホーム画面を抜けて Welcome まで戻る必要があり遠回りでした。

本 issue では、ホーム画面のコマンドモードに `config` コマンドを追加し、ワンアクションで Config 画面へ遷移できるようにします。Config 画面を抜けると元のワークスペース画面へ戻ります。

## やること

- ホーム画面のコマンドモードに `config` コマンドを追加し、`Effect::OpenConfig` を返す。
- `config` 実行で Config 画面（設定編集）へ遷移する。編集対象は従来どおりグローバル設定 ＋ 現在のワークスペースのローカル上書き（`<workspace>/.usagi/settings.json`）。
- Config 画面で `Esc` / `q`（Back）を押すとワークスペース画面へ復帰する。`Ctrl+C`（Quit）はアプリ全体を終了する。

## 完了条件

- ホーム画面で `:config` を実行すると Config 画面が開く。
- Config 画面を Back で抜けると元のワークスペース画面へ戻り、Quit でアプリが終了する。
- 編集対象が「起動中のワークスペースのローカル設定」になっている。

## 実装状況

Welcome から Config を開く既存の経路（`config::run`）を、明示的なプロジェクトコンテキストを受け取れるよう一般化して再利用。

- `presentation/tui/home/command.rs`：`config` コマンドを追加し、`Effect::OpenConfig` を返す。
- `presentation/tui/home/event.rs`：`Effect::OpenConfig` を受け、注入された `open_config` コールバックで設定画面へハンドオフする。Quit が返れば `Outcome::Quit` を伝播し、Back ならワークスペース画面を再開する。
- `presentation/tui/config/mod.rs`：`run_in(term, repo_root)` を追加し、編集対象リポジトリを明示できるようにした。従来の `run` は現在のリポジトリを渡して `run_in` に委譲（ワイヤリングのためカバレッジ計測対象外）。
- `presentation/tui/home/mod.rs`：起動中ワークスペースのパスを `config::run_in` へ渡す `open_config` を組み立て、`event_loop` へ注入する（ワイヤリングのためカバレッジ計測対象外）。
