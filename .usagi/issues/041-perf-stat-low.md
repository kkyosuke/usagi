---
number: 41
title: perf: 軽微なアロケーション・stat 重複の整理（Low）
status: done
priority: low
labels: [perf]
dependson: []
related: []
created_at: 2026-06-17T22:51:09.051216+00:00
updated_at: 2026-07-11T01:07:32.783411+00:00
---

## 背景

規模が大きくなると効いてくる、または局所的な低優先のパフォーマンス改善をまとめて扱う。

- **`render_grouped` の stats 二重計算**（`src/presentation/cli/issue/render.rs:22-39`）— overall と各グループで合計 ~2n 走査。グループ集計時に overall も足し込めば 1 パス。
- **`tree.rs:30-41` で starts に全ノードを 2〜3 重 push** — visited で吸収されるが中間 Vec が ~2n に膨張。最後の「全件」extend だけで網羅は足りる。
- **`exists()` + 直後 `create_dir_all`/`read_dir`**（`issue_store.rs:188`、`memory_store.rs:172`、`error_log.rs:78`）— stat が重複。`create_dir_all` は冪等、`read_dir` の `NotFound` を分岐で扱えば stat を省ける。
- **キー 1 打ごとに write+flush**（`pty.rs:218`）— `pump_input` でバイトを貯めてループ終了時に 1 回 flush。
- **`from_utf8_lossy().trim().to_string()`**（`git/command.rs:51`）— Cow→再確保し、直後に `lines()` で再分割。
- **`format_number_list` の中間 Vec**（`src/domain/issue/markdown.rs:155-164`）— `to_markdown` のたびに一時 Vec。直接 `write!` で回避。
- **workspace 操作の全 load→save**（`src/usecase/workspace.rs`）— `touch` が最終使用時刻更新だけで全ファイル書き換え。

## 確認方法

- 既存テストが通ること（カバレッジ 100% 維持）。各項目は独立に着手可。
