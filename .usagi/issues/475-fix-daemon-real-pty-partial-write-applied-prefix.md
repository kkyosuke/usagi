---
number: 475
title: fix(daemon): real PTY partial write の applied_prefix を正確に返す
status: in-progress
priority: high
labels: [review, v2, daemon, pty, ipc]
dependson: []
related: [218, 264, 271]
parent: 453
created_at: 2026-07-20T12:06:24.693479+00:00
updated_at: 2026-07-20T21:49:49.060337+00:00
---

## 問題・影響

root/v2 の `crates/daemon/src/infrastructure/pty.rs` にある `PtyTerminal` の `PtyWriter::write_all` 実装は `std::io::Write::write_all` の error を常に `PtyWriteError { applied_prefix: 0 }` へ変換する。実際に prefix を PTY へ書いた後の failure でも 0 と報告し、`TerminalRegistry::write_input` が安全な retry と ambiguous write を区別できず二重入力を起こす。

## 成立条件 / 再現フロー

writer が N bytes 成功後に error/`WriteZero` を返すよう fault injection し、同じ input operation を retry する。applied prefix 0 のため全 bytes が再送され、shell/Agent に prefix が二重適用される。

## 対象責務と非対象

実 PTY writer の明示 write loop、`Interrupted`、partial/error の byte accounting と registry outcome mapping を対象とする。input protocol 全体の変更、terminal reconnect は非対象。

## 受入条件

- [ ] 各 successful `write` の byte 数を加算し、error 時に正確な `applied_prefix` を返す。
- [ ] `Interrupted` は progress を失わず再試行し、`WriteZero` は現在 prefix を伴う failure とする。
- [ ] prefix 0 の safe retry、全量 success、prefix >0 の `Ambiguous` を registry/IPC まで保持する。
- [ ] 同一 operation retry で既適用 bytes を暗黙に二重送信しない。

## 必須回帰テスト

scripted partial writer で partial→error、複数 partial、Interrupted、WriteZero、full success を固定し、実 PTY path の operation replay が ambiguous/safe を正しく返すことを検証する。

## docs / 移行影響

`document/04-ipc.md` に terminal input の partial/ambiguous outcome を記載する。wire 型が既存 prefix を持つため migration は原則不要。
