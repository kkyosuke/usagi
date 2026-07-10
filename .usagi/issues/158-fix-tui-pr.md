---
number: 158
title: fix(tui): PR リンク検知の取りこぼしを塞ぐ（可視ビューポート限定スキャン → 履歴スキャン化）
status: todo
priority: high
labels: [fix, tui]
dependson: []
related: []
created_at: 2026-07-09T23:29:32.568343+00:00
updated_at: 2026-07-09T23:29:32.568343+00:00
---

## 背景（調査結果）

usagi は GitHub を叩かず、埋め込み端末のライブ出力から PR URL をスクレイプして sidebar の `#N` バッジにする（正本: `src/infrastructure/pr_link_store.rs` 冒頭 / スキャナ `src/presentation/tui/home/terminal/link.rs`）。反映経路は 2 系統ある。

- **バックグラウンド/デタッチ**: watcher スレッド（`terminal/pool.rs`、`POLL_INTERVAL = 200ms`）が `pending_pr_scans` → `scan_pr_jobs` で `link::pr_links(parser.screen())` を実行し、`pr_link_store::add` 後に `take_pr_link_updates()` 経由で sidebar へ。
- **没入(Attached) 自ペイン**: `terminal/pane.rs` の描画ブロック内（`links_cache.gen != gen` の fresh-output フレーム）で `link::scan_links(screen)` → `pr_links_from(scan.urls)` を実行し `add`/`get`/`set_pr_links`。他セッション分は `pane.rs:463` で watcher の更新を drain（#680 の考え方）。

### 遅延（実測ベースの見積り）

| ケース | 遅延要因 | 概算 |
|---|---|---|
| 没入・自ペイン | 次の描画フレーム（`MIN_FRAME = 16ms` スロットル）| ほぼ即時（≤16ms） |
| バックグラウンド/デタッチ | watcher の `POLL_INTERVAL = 200ms` ポーリング + drain 1 フレーム | ≤ ~200ms |
| 没入・他セッション | 同上（watcher 200ms → attached ループが drain）| ≤ ~200ms |

**結論: 遅延そのものは有界で許容範囲**（自ペインは実質即時、背景は最大 ~200ms）。#680 が入れた attached ループでの drain とも整合している。**主たる欠陥は遅延ではなく「取りこぼし」**。

## 症状（取りこぼし）

PR URL が端末に出たのに sidebar に `#N` が**一切出ない**ことがある。scrape 設計上、取りこぼした URL を後から拾う経路は無い（再表示されない限りバッジは永久に出ない）。

## 根本原因

両スキャン経路とも **vt100 の「可視ビューポート」だけ**を走査する（`parser.screen()` → 内部で `grid.visible_rows()`。`link.rs` の `scan_links`/`pr_links` は `screen.size()` の可視行のみをフラット化）。スクロールバック（`third_party/vt100/src/grid.rs` の `scrollback: VecDeque<Row>`）は走査対象外。

スキャンはスナップショット方式で、かつ

- **バックグラウンドは 200ms 間隔**でしか走らない。
- reader スレッドは 1 回の `read` で最大 **64 KiB** を `parser.process()` に流す（`src/infrastructure/pty.rs`）。1 チャンクで大量行がスクロールしうる。
- `pending_pr_scans` は generation を観測して `last_generation` を即更新するため、2 スキャン間に発生した中間スクリーン（URL が見えていた瞬間）はスキップされる。
- 自ペインもスロットル中（`output_changed && !throttled` 不成立）は描画＝スキャンが繰り延べられる。

このため **PR URL が印字された直後に後続出力で可視領域外へスクロールする**（例: エージェントが PR URL を出した直後にサマリや status フッターを流す）と、次にスキャンが走った時点で URL は既にスクロールバックへ落ちており、**どの可視スクリーンにも現れず永久に取りこぼす**。フルスクリーン TUI／alt-screen で URL が一瞬表示→上書きされる場合も同根で取りこぼす。

## 変更方針

PR URL の収集を「可視スクリーンのスナップショット走査」から「**出力履歴（scrollback + visible）の走査**」へ変える。vt100 は vendor 済み（`third_party/vt100`）なので、可視だけでなく**前回スキャン以降に流れた行**まで拾えるようにする。

1. **vendored vt100 に最小 API を追加**
   - スクロールバック行数と、これまでにスクロールアウトした累積行数（単調増加の high-water）を公開するアクセサ。
   - 指定範囲のスクロールバック行 + 可視行のテキスト（論理行の折返しは既存 `row_wrapped` 相当で結合）をイテレートする手段。
   - 既存の `visible_rows`/描画・選択のセマンティクスは不変に保つ（scrollback_offset を書き換えない読み取り専用 API にする）。
2. **純粋なハーベスタを追加**（`link.rs` か新規 `pr_harvest.rs`）: `(screen, last_watermark) -> (new PrLinks, new_watermark)`。可視行は毎回走査（画面高ぶんで安価）、スクロールバックは**前回以降に増えた行だけ**走査してコストを抑える（既定 `terminal_scrollback_lines = 2000` を毎回全走査するとパーサロック保持時間が伸びるため不可）。URL 抽出は既存 `url_spans`/`parse_pr_url` を再利用。
3. **2 経路を張り替え**: watcher（`PrScanJob`/`scan_pr_jobs`）と自ペインの両方でハーベスタを使い、per-pane に watermark を保持。パーサロック保持時間は現状（可視スキャン + off-lock 永続化）と同等以内に収める。永続化・dedup（`pr_link_store` の accumulate）・`set_pr_links`・generation バンプは現行踏襲。

## 受け入れ条件

- PR URL が端末に現れた直後（自ペインは次フレーム、背景は次 watcher tick ≤200ms）に対象セッション行へ `#N` が出る。
- **URL が印字直後に可視領域外へスクロールしても取りこぼさない**（scrollback に残る限り拾う）。フルスクリーン TUI が URL を一瞬表示するケースも拾える。
- 既存挙動の退行なし（可視スキャンのテスト・PR URL のパース仕様は不変）。パーサロックの臨界区間を有意に長くしない。
- クリーンアーキの依存方向維持（presentation→infrastructure は正）。実 IO は注入し**カバレッジ 100%**（新ハーベスタ・vendored vt100 の新 API・張り替えた 2 経路に対しテスト追加）。
- vt100 パッチの意図（`Cargo.toml` の vendored 説明）と矛盾しないこと。

## 非目標

- バックグラウンドの ~200ms 遅延短縮（イベント駆動での watcher 起床など）。遅延は有界で許容範囲のため本 issue のスコープ外。必要なら別 issue。
- scrollback 上限（既定 2000 行）を超える猛烈な出力フラッシュで、印字と同 tick 内に URL が evict される極端ケース。実運用ではほぼ起きないため許容し、必要なら別途検討。
- pool.rs の責務分割（#128 の範囲）。本 issue はロジック修正に限定する。
