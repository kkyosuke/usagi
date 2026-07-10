---
number: 174
title: perf(tui): vt100 scrollback の実確保を削減する（行の末尾空白トリム・非表示セッションの縮退）
status: todo
priority: low
labels: [perf, tui]
dependson: []
related: [159, 172]
created_at: 2026-07-10T20:47:45.559154+00:00
updated_at: 2026-07-10T20:47:45.559154+00:00
---

## 背景（メモリ調査 2026-07-11）

ペインごとの vt100 grid は `(rows + scrollback) × cols × ~32B/cell`（Cell = `[u8;22]` contents + Attrs ≈ 32B）。scrollback は `Settings::terminal_scrollback_lines`（既定 2,000・上限 50,000）で有界だが、vendored fork `third_party/vt100` の `Row::new` は **1 行につき全 cols 分の `Vec<Cell>` を即時確保**する。端末出力は短い行が大半のため、実コンテンツに対して大きく過剰確保になる（既定 120 cols なら空行 1 行でも ~3.8KB、scrollback が埋まると 1 ペイン ~7.8MB が丸ごと実確保）。

`TerminalPool` は全セッションの全ペインを常駐させるため、この確保はペイン数に線形で効く（セッション 10×2 ペインで ~150MB オーダー）。

## やること（効果順の候補、いずれも vendored fork なので改変可能）

1. **scrollback 行の末尾空白セルをトリムして保持する**: scrollback へ押し出される行（以後ほぼ不変）だけでも、末尾の空 Cell を落として `Vec` を shrink する。読み出し時は不足分を default Cell として扱う。典型出力で scrollback メモリを数分の一にできる見込み。visible grid は書き込みが続くため対象外にして複雑さを抑える。
2. **非表示（バックグラウンド）セッションの縮退**: 表示中でないセッションのペインは preview に必要な可視領域だけ残し、scrollback を圧縮/休止する案。切替時の scrollback 復元性とのトレードオフがあるため、1 で足りなければ検討。
3. （小）ペイン種別（agent / shell）やセッション状態ごとに scrollback 行数を分ける設定。

## トレードオフ・関連

- fork（`third_party/vt100`）の diff が増える。reflow（resize 時の再折返し）との整合に注意。
- #172（ended ペインの解放）が先に効く: ended 分は解放、live 分は本 issue で圧縮、という住み分け。
- #159（daemon 化）後は grid の権威が daemon に移るため、同じ圧縮を daemon 側 vt100 に適用する。

## 確認方法

- 長い出力を流した後のペインあたり RSS 寄与が改善前より減ること（ベンチ or 手動計測）。
- scrollback の表示・コピー・reflow の既存テストが通ること。カバレッジ 100% 維持。
