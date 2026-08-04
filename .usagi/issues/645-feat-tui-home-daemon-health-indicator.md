---
number: 645
title: feat(tui): Home に診断専用の daemon health indicator を出す
status: done
priority: medium
labels: [v2, tui, core, metrics, diagnostics]
dependson: []
related: []
created_at: 2026-08-04T12:57:36.226819+00:00
updated_at: 2026-08-04T13:33:30.340517+00:00
---

## 目的

Home の mascot sidecar は daemon の CPU / 常駐メモリだけを出しており、**daemon 側の劣化（metrics lane の停止、
端末出力の欠落、PR 検出の取りこぼし）は画面のどこにも出ない**。利用者は「うさぎは動いているのに端末の履歴が
飛ぶ」「PR modal に出ない」という症状だけを見ることになり、原因が daemon 側の飽和なのか自分の操作なのか
区別できない。

**正常時は今の画面と 1 バイトも変えず、本当に異常または要注意のときだけ**短い理由付き indicator を出す。

## 調査（既存 metrics の意味と actionable かどうか）

`DaemonMetrics`（`crates/core/src/usecase/client.rs`）の各 counter を daemon 側の実装まで辿った結果は次のとおり。

| field | 実際の増加契機 | 単独の増加は異常か |
|---|---|---|
| `dropped_updates` | metrics broker の 1 slot が埋まったまま次の tick が来た（購読側が遅い） | いいえ。1 回なら表示遅延だけ |
| `terminal_dropped_bytes` | 端末 1 本の retention journal が 64KiB（`MAX_RETAINED_OUTPUT_BYTES`）を超えて古い側を捨てた | **いいえ。ring buffer の通常動作**。忙しい agent では常時増える |
| `terminal_coalesced_bytes` | 直前 segment への追記で結合した | いいえ。省メモリの成功側 |
| `terminal_backpressured_bytes` | PTY reader が bounded queue の空きを待った | いいえ。バースト時は普通に起きる |
| `pr_projection_dropped_bytes` / `pr_projection_gaps` | 遅延 PR projection queue が満杯で、確定済み出力を **PR scan せずに捨てた** | はい。機能の取りこぼしで、queue 満杯は通常動作ではない |
| （metrics 不通） | lane 失敗中は composition root の port が**直前 sample を保持し続ける**（`src/runtime/tui.rs`） | 見分けるには **freshness（`sampled_at_ms`）** が必要 |

重要な発見が 2 つある。

1. **`terminal_dropped_bytes` の「増加」を異常として出すと恒常的に赤くなる**（64KiB の retention window を
   超えるだけで増える）。cumulative 値や単純な差分 > 0 では判定にならない。
2. **metrics lane の失敗は snapshot の欠落として観測できない**。port が直前 sample を保持するため、
   `Option<DaemonMetrics>` は `Some` のままになる。停滞は `sampled_at_ms` と現在時刻の差でしか分からない。

また `daemon が居なければ metrics 無しで動作する`（`document/03-tui.md`）ため、**一度も観測していない状態は
正常**であり、赤くしてはならない。既存の `waiting daemon` shimmer がその状態を既に表現している。

## 設計

### 権威ではない

health は**診断専用の projection** である。`AppState` にも reducer にも持たせず、Effect も出さず、
どの command の可否・ownership・fence 判定にも参加しない。判定材料は daemon が出した表示専用 counter だけで、
永続化もしない。

### 判定（pure/testable）

新規 module `usagi-core` の `usecase::daemon_health` に閉じる。既存の巨大ファイル（`presentation/mod.rs` /
`views/workspace.rs`）へロジックを足さないので、並行タスクとの競合も小さい。

- `DaemonHealthTracker::observe(&DaemonMetrics)` が sample 間の差分を畳む。時計は sample 自身の
  `sampled_at_ms` だけを使い、IO も実時計も触らない。
- `DaemonHealthTracker` は `Clone + Copy + PartialEq + Eq` で frame material に載る値であり、
  `tracker.evaluate(now_ms) -> DaemonHealth` が freshness と合成して level / 理由を返す純関数である
  （実時計は renderer が受け取る `now` から渡す）。
- 理由は**閉じた enum**（`HealthReason`）で、free text を持たない。したがって secret / raw PTY 出力 / path が
  indicator に載ることが構造的に起こり得ない。表示文言は presentation 側の対応表で決める。

| 判定 | 条件 | level |
|---|---|---|
| （静か） | 一度も観測していない（daemon 不在・lane 未起動） | ok |
| `DaemonUnresponsive` | 観測済みで、最新 sample が 30s 以上古い | danger |
| `MetricsStalled` | 観測済みで、最新 sample が 6s 以上古い | warning |
| `TerminalOutputDropped` | `terminal_dropped_bytes` が 1MiB/s 以上を 3 sample 連続 | warning |
| `TerminalBackpressure` | `terminal_backpressured_bytes` が 256KiB/s 以上を 3 sample 連続 | warning |
| `PrScanIncomplete` | `pr_projection_dropped_bytes` / `pr_projection_gaps` が増えた | warning |
| `MetricsUpdatesDropped` | `dropped_updates` が 1/s 以上を 3 sample 連続 | warning |

cumulative counter を「一度増えたら永遠に赤」にしないための規則。

- **rate で見る**。差分を sample 間の経過 ms で割った毎秒レートを閾値と比べる。単発のバーストは通らない。
- **連続で見る**。閾値超えが 3 sample 続いて初めて点灯する（PR 取りこぼしだけは 1 回で点灯する。queue 満杯は
  通常動作ではないため）。
- **減衰する**。点灯は最後の該当 sample から 10s で自然に消える。事象が続いていれば点き続ける。
- **再 baseline する**。counter の後退（daemon 再起動 = 別 process の broker）、`sampled_at_ms` の後退、
  `schema_version` の変化、5s を超える観測の空白（再接続直後）では、差分を取らず baseline だけ引き直す。
  したがって再接続の 1 発目が警告になることはない。
- freshness の理由は counter の理由より優先する。停滞している snapshot から出したレートを表示しない。

### 表示

Home 左 sidebar の mascot sidecar に 1 行追加する。sidecar は既に**うさぎの 3 行に対して最大 3 行**を許して
おり、現在 1 行（CPU / メモリ）しか使っていないため、**mascot の予約行数（`reserved_rows`）は変わらない**。
health が ok のときは行を足さないので、正常時の frame は現在と完全に同一である。

- 文言は `⚠ ` + 短い理由（例 `⚠ metrics 停滞`）。**Nerd Font glyph を新たに使わない**（`⚠` は既存の
  mascot bubble で使っている BMP glyph）。
- danger は danger role、warning は warning role の style を使う。
- 狭幅は sidebar 幅から予算を計算して段階的に縮退する。文言が入らなければ `⚠` 1 文字だけ、予算 0 なら行を出さない。
- metrics unavailable（`None`）でも health が ok でなければ badge 行だけを出す。
- daemon 切断・再同期要求は既存の [feedback](../../document/03-tui.md#feedback-と終了) と mascot bubble が正本で、
  health は二重に報告しない。health が持つのは「観測できているか（freshness）」と counter 由来の劣化だけである。

## 受け入れ条件

- 正常時（health ok）の Home frame が現在と同一である（回帰テストで固定する）。
- 一度も観測していない状態で warning / danger を出さない。
- cumulative counter が増えたあと事象が止まれば indicator が消える。
- daemon 再起動・再接続直後の 1 sample で点灯しない。
- 狭幅・metrics 不在で panic せず、幅を溢れない。
- `document/03-tui.md` を実装に合わせて更新する（記載＝実装済み）。

## テスト方針

- `usagi-core` の `usecase::daemon_health`: 各 level / 理由 / 優先順位、rate 未満、連続未達、減衰、counter 後退、
  sample 時刻後退、schema 変化、観測空白、0 除算（同一 `sampled_at_ms`）を純関数の単体テストで固定する。
- `usagi-tui`: sidecar 行の生成（ok = 追加行なし、warning / danger の文言と style、狭幅縮退、metrics `None`）、
  `MetricsProjection` が drain から tracker を進めること、Home frame が正常時に不変であること。
