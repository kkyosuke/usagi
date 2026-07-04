---
number: 103
title: fix(tui): シグナル終了時にマウストラッキング等の端末モードを復元する
status: done
priority: high
labels: [bug, tui]
dependson: []
related: []
created_at: 2026-07-04T07:28:05.317131+00:00
updated_at: 2026-07-04T07:41:40.938010+00:00
---

## 目的

usagi を終了した後、ホストシェルにマウスレポート（`^[[<35;70;25M` のような列）が流れ込む不具合を直す。マウスを動かすたびに `\x1b[<btn;x;yM` が出続け、シェルが表示可能なゴミとしてエコーする。

## 原因

画面に出るのは **SGR マウストラッキング（DECSET 1006）+ any-event motion（1003）** のレポート。usagi 終了後もマウスモード（1000/1002/1003/1006）が有効なまま残っているため、シェルがマウス移動のたびに受け取るレポートをエコーしている。

マウスモード解除シーケンス（`DISABLE_MOUSE`）を書き出す経路は次の 2 つだけ:

1. `AlternateScreenGuard::Drop`（`src/presentation/tui/io/screen.rs`）— 正常終了・panic アンワインド時
2. `install_panic_hook`（`src/presentation/tui/app/mod.rs`）— panic のバックストップ

**シグナルハンドラが存在しない**ため、Drop を経由しないプロセス終了で端末が汚れる。特に:

- `cargo run` 下での **Ctrl-C**: `RawModeGuard`（`src/presentation/tui/io/term_reader.rs`）は `select()` 待ち受け中しか raw モードを保持しない。レンダリング中など raw モードを持っていない一瞬に Ctrl-C を押すと、SIGINT が前景プロセスグループ（cargo と usagi）に届き、usagi は `Key::CtrlC` の正常終了ではなく本物の SIGINT で即死する → RAII の Drop が走らず解除シーケンスが出ない。
- `kill`（SIGTERM）/ 端末・SSH を閉じる（SIGHUP）でも同様に Drop はスキップされる。

## 変更内容

- panic hook（`app/mod.rs` 付近）が書き出している端末復元バイト列
  `\x1b[?1049l\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?1003l\x1b[?2004l\x1b[?25h`
  を共通のヘルパ関数に切り出す（テスト可能にするため、書き込み先は `impl Write` で注入できる形に）。
- **SIGINT / SIGTERM / SIGHUP** のハンドラを追加し、上記の復元バイト列を stdout（fd）へ書き出してから既定の終了挙動へ進む。
  - シグナルハンドラ内では async-signal-safe な最小限の処理（生 fd への `write`）に留める。
  - raw mode の解除も忘れずに。
- 復元バイト列を組み立てる純粋関数はユニットテストで正確なバイト列を検証する（panic hook と共有していることを担保）。

## テスト・確認方法

- `cargo fmt` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（カバレッジ 100% 維持）。
- 復元バイト列生成関数のユニットテスト。
- 手動確認: `cargo run` で TUI を起動し、レンダリング中に Ctrl-C（または別端末から `kill -TERM <pid>` / `kill -HUP <pid>`）で終了させ、その後シェルでマウスを動かしてもレポートのゴミが出ないこと。

## ドキュメント

端末モードのライフサイクルに関わる記述があれば `document/design/` 等を実装に合わせて更新する。
