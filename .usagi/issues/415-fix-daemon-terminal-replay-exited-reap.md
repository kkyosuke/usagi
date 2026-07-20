---
number: 415
title: fix(daemon): terminal replay バッファを有界化し exited 端末の reap 経路を追加する
status: todo
priority: high
labels: [fix, daemon, review]
dependson: []
related: []
created_at: 2026-07-20T11:55:47.327941+00:00
updated_at: 2026-07-20T11:55:47.327941+00:00
---

## 背景

v2 全体の 7 サブシステム並列コードレビュー（2026-07-20）由来。file:line は 2f4dc5b6 時点で検証済み。

## 根拠（検証済み）

- `crates/daemon/src/usecase/terminal.rs:305` — `entry.replay.extend_from_slice(&data);` に上限がない。`replay: Vec<u8>`（定義 :169、初期化 :227）は一度も切り詰められない（有界なのは別フィールドの `journal`/`retained_bytes` :314-320 のみ）。
- `terminal.rs:467` — `snapshot()` が Attach/Resync のたびに `replay: entry.replay.clone()` で**全量 clone**する（free fn `snapshot` :461、`pub fn snapshot` :436 から呼ばれる）。
- `TerminalRegistry`（:187）に `remove`/reap 相当のメソッドが存在しない。exited エントリも registry に残り続ける。
- 合成ルート `src/runtime/daemon.rs:139-142` の `DiscardJournal` コメントは "The registry's **bounded** in-memory replay buffer already serves reconnect within retention…" と記述しており、実装（無制限）と矛盾。

## 問題

長寿 agent の出力が daemon 常駐メモリに永久保持され、Attach/Resync のたびに全量 clone される。出力量に比例してメモリと attach レイテンシが劣化し、exited 端末分も回収されない。

## 改善案（要検討）

- replay を journal と同様の有界リングにする。切り捨てが発生した接続には ResyncRequired 相当（全画面再送要求）を返す。
- exited 端末の reap（registry からの削除）経路を追加する（一定猶予後 or 明示 verb。reconcile verb 配線 issue と関連）。
- `DiscardJournal` コメントを実装に一致させる。

## 受け入れ条件

- [ ] replay バッファに上限があり、超過時の挙動（切り捨て＋Resync 要求）がテストで固定されている。
- [ ] exited エントリを registry から回収する経路が存在する。
- [ ] `src/runtime/daemon.rs` の "bounded" コメントが実態と一致している。
- [ ] coverage 100% を維持する。
