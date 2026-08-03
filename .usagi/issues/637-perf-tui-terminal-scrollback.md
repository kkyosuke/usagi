---
number: 637
title: perf(tui): terminal 出力時の scrollback 全再構築を差分化する
status: done
priority: medium
labels: [review, v2, tui, terminal, performance, rendering]
dependson: []
related: [389, 527, 535, 554, 587]
created_at: 2026-08-02T23:42:45.638393+00:00
updated_at: 2026-08-03T01:31:01.363451+00:00
---

## 問題・発生条件

foreground の live terminal が新しい PTY 出力を受け取るたび、TUI は最大 10,000 行の retained scrollback 全体を描画文字列とリンク索引へ同期再構築する。RPC 自体は [#527](527-perf-tui-terminal-polling-ui-loop-foreground-cadence.md) で背景 pump へ移ったが、completion の適用と cache rebuild は frame loop の描画・入力 thread に残る。

発生経路は次の 1 本である。

```text
16 ms Home loop
  → close_exited_panes
  → WorkspaceUi::poll_all_terminals
  → TerminalSession::poll / apply_at
  → changed=true
  → refresh_display_cache
  → TerminalScreen::rows_with_scrollback_and_cursor
```

長時間動く Agent / Terminal が retained history を蓄えた状態で出力を続けると、chunk completion を drain するたびに発生する。

## コード根拠と規模

- `crates/tui/src/usecase/application/terminal_session.rs`
  - `TerminalSession::apply_at` は受信 chunk を `VtScreen::advance` した後、1 chunk 以上あれば `refresh_display_cache()` を同期実行する。
  - `refresh_display_cache` は live state で `TerminalScreen::rows_with_scrollback_and_cursor()` を呼び、`display_cache` 全体を置換する。
  - viewport 投影側の `display_row_window` は必要な行だけ clone するが、それは全履歴 cache を作り終えた後である。
- `crates/tui/src/usecase/application/terminal_screen.rs`
  - `rows_with_scrollback_and_cursor` は最初に `link_cells()` を呼ぶ。
  - `link_cells` は scrollback + visible grid の全行を ANSI-free `Vec<String>` にし、`terminal_link::scan_links` へ渡す。
  - その後、同じ全行を `render_row_selected` で再走査し、ANSI 付き `Vec<String>` を作る。
- `crates/tui/src/usecase/application/terminal_link.rs`
  - `scan_links` は `expand` で全行を display-column の `Vec<Vec<char>>` へ展開してから、logical line を再度全走査する。
- `crates/core/src/usecase/vt_screen/checkpoint.rs`
  - `SCROLLBACK_MAX = 10_000`。端末幅を `C`、retained row 数を `R` とすると、1 回の invalidation が時間・作業 allocation とも `O(R × C)` である。
- `src/runtime/terminal_pump.rs` / `src/runtime/tui.rs`
  - foreground pump は出力中 8 ms から、無出力時 64 ms まで backoff する。Home tick は 16 ms なので、連続出力時の cache rebuild は UI drain の最大約 62.5 回/秒に追随しうる。

cache 自体は idle tick ごとの全履歴再構築を避けるが、invalidation が「変更行」ではなく「全履歴」であるため、出力が続く場合は履歴長に比例した仕事を毎 completion で繰り返す。

## 実測

tracked file を変更せず、`TerminalScreen` の公開 API を使う release 一時 probe（24×80、同一 screen を20回再投影）で計測した。値は Home projection、Home render、`Frame::from_lines`、diff、terminal write を含まない focused lower bound である。

| retained rows | 通常行・20 rebuild | 各行に URL・20 rebuild | 1 rebuild |
|---:|---:|---:|---:|
| 100 | 2.30 ms | 6.38 ms | 0.12–0.32 ms |
| 1,000 | 22.09 ms | 74.55 ms | 1.10–3.73 ms |
| 10,000 | 211.97 ms | 860.02 ms | **10.60–43.00 ms** |

10,000 行では通常出力でも cache rebuild だけで 16 ms frame budget の約 66%、URL が多い履歴では約 2.7 倍を消費する。ここへ Home projection / ANSI frame parse / diff / write が加わる。

## 影響

- retained history が長い pane で出力が来るたび、draw / input / modal / quit の前に 10–43 ms 以上を同期消費し、入力遅延と frame drop が可視化する。
- 連続出力では同じ不変 scrollback を毎秒最大約 62 回文字列化・column 展開・URL scan し、CPU と allocator bandwidth を消費する。
- terminal 出力が増えるほど現在表示中の末尾数十行ではなく過去 10,000 行のコストを払い続けるため、長時間 Agent を使うほど悪化する。

## 既存 issue との境界

- [#389](389-v2.md) は terminal 全体のリンク可視化・クリックを導入した。本 issue はその挙動を維持しつつ、リンク索引の invalidation 粒度を小さくする。
- [#527](527-perf-tui-terminal-polling-ui-loop-foreground-cadence.md) は foreground `Resume` RPC を UI loop から分離した。本 issue は背景 completion を UI state に適用した後の同期 projection cost を扱う。
- [#535](535-fix-tui-checkpoint-negotiation-screen-reconstruct-legacy-fail-closed.md) は attach/resync の semantic checkpoint reconstruct を扱う。checkpoint の完全 invalidation は必要だが、steady output の全 invalidation は別問題である。
- [#554](554-perf-tui-frame-io.md) は material 不変 tick の render/draw を skip した。terminal output は material を変えるうえ、本件の cache rebuild は material 比較より前に実行される。
- [#587](587-perf-tui-frame-loop-notify-all-terminal-view-clone.md) は viewport `terminal_view` の不要 clone を除いた。全履歴 `display_cache` の再生成は対象外である。

## 修正方針

- terminal の render/link cache を incremental にする、または immutable scrollback と mutable visible grid を分離する。
- append / cell update では変更行と、折返し URL の判定に必要な隣接 logical line だけを再 scan / rerenderする。scrollback eviction 時も先頭削除を全再構築へ直結させない。
- resize、semantic checkpoint replace、selection 開始/更新、cursor/SGR 変更はそれぞれ必要な invalidation 範囲を明示する。resize/checkpoint の full rebuild は許容しても、通常 chunk append と同じ扱いにしない。
- viewport、pointer hit-test、link underline/open、selection/copy が同じ retained-cell identity を参照する現在の correctness は維持する。
- 1 completion で frame budget を超えうる仕事が残る場合は、projection の予算分割または背景 worker 化を行い、terminal revision/cursor で fence して古い projection が新しい screen を上書きしないようにする。

## 受入条件

- 10,000 行近い history への通常 append が、全 10,000 行ではなく変更行と必要な wrapped-neighbor 数に比例して cache を更新する。
- URL underline / click、wrap を跨ぐ URL、cursor、SGR、CJK 幅、selection highlight、copy の表示・hit-test parity が維持される。
- resize と checkpoint replace は必要な full invalidation をちょうど1回行い、その後の append は incremental path に戻る。
- burst 内の複数 chunk は同じ screen revision に対して coalesce され、chunk 数 × 全履歴走査にならない。
- 長い history で連続出力中も input / modal / quit が frame loop 上の全履歴 rebuild に block されない。
- カバレッジ 100% を維持し、実装した cache/invalidation 契約を `document/03-tui.md` の live terminal 節へ反映する。

## 必須回帰テスト・計測

- visited/rendered row 数を数える fake/instrumented cache で、10,000 行への 1 行 append が全履歴ではなく変更行 + wrapped logical neighbors だけを処理することを assert する。
- URL が変更境界の前後で wrap する追加・上書き・eviction を固定し、link cells と click URL が full rebuild の reference と一致することを assert する。
- cursor-only / SGR / CJK / selection / copy / resize / checkpoint replace ごとの invalidation matrix を固定する。
- 1 completion に複数 contiguous chunk がある burst で、cache publish が coalesce されることを assert する。
- 100 / 1,000 / 10,000 行の append benchmark を残し、append の latency が retained row 数に線形増加しないことと frame budget を記録する。
- 実 PTY E2E で長い出力を流しながら input、modal、quit が所定時間内に完了することを確認する。

## 実装時の決定

### 全履歴 cache ではなく viewport 遅延投影を採用

`TerminalSession` の `display_cache: Vec<String>` を撤去した。PTY chunk の適用、resize、接続状態変更は `VtScreen` だけを更新し、retained history の ANSI 文字列化や URL scan を行わない。

Home が描画素材を要求するときは、まず末尾の有効行数を文字列 allocation 無しで求め、`display_row_window(start, end)` が要求された retained row だけを投影する。URL が viewport 境界を跨ぐ場合は、従来と同じ wrap 判定でその logical line の先頭・末尾まで scan 範囲を広げる。したがって通常経路は viewport 行数に比例し、長い wrapped line だけが正しさに必要な範囲を追加で払う。

selection / copy は明示操作時に untrimmed cells 全体を snapshot する既存契約を残した。これによりdrag中の出力で選択対象が動かず、通常出力へ全履歴costを戻さない。

### 実測（実装後）

起票時と同じ release probe（24×80、20回のviewport投影）では、retained row数に依存しない結果になった。

| retained rows | 通常行・20 viewport | 各行に URL・20 viewport |
|---:|---:|---:|
| 100 | 0.520 ms | 1.685 ms |
| 1,000 | 0.522 ms | 1.714 ms |
| 10,000 | 0.506 ms | 1.793 ms |

10,000行の1回あたりは通常約0.025 ms、URLあり約0.090 ms。起票時のfull rebuild（10.60–43.00 ms/回）に対し約420–480倍短く、履歴長による線形増加も消えた。

### 回帰固定

- 10,000行のunwrapped historyで24行windowのscan範囲が24行だけであることをassertした。
- wrapped URLの途中から始まるwindowがfull projectionの同じsliceと一致し、scanがlogical-line先頭まで広がることをassertした。
- blank live cursorを有効行数へ含め、非liveのblank paddingを除く契約をassertした。
- 既存のterminal screen/session、link click、selection/copy、checkpoint/resize、CJK/SGR parity testsを通した。
