---
number: 695
title: feat(core): v2 初回起動時に v1 の workspace 一覧を read-only で seed する
status: todo
priority: medium
labels: [core]
dependson: []
related: [693, 694]
created_at: 2026-08-17T23:20:50.501261+00:00
updated_at: 2026-08-17T23:20:50.501261+00:00
---

設計は [document/proposals/17-v2-trial-channel.md](../../document/proposals/17-v2-trial-channel.md) の
「P3: 新しい UI を空にしない」が正本。

## 背景

v2 の runtime mode は既定 `local` なので global state は `<base>/local/` にある。v1 の
`<base>/workspaces.json` とは分離されており、これが**試用が v1 を壊さない理由**である。
一方その裏返しとして、v2 を初めて起動すると **workspace 一覧が空**で始まる。
「新しい UI を試す」で空の画面が出ると壊れて見えるため、初回だけ v1 の一覧を seed する。

## やること

| 性質 | 内容 |
|---|---|
| 方向 | 一方向。v1 の `<base>/workspaces.json` を**読むだけ**。書かない・移動しない・rename しない |
| 回数 | **1 回だけ**。marker を置いて再実行しない（再実行すると v2 で削除した workspace が復活する） |
| 失敗時 | seed に失敗しても起動を止めない。空の一覧で開き、error log に残す |
| 対象 | workspace の登録と最終利用日時のみ。**settings は seed しない**（v1 と v2 で schema と項目が異なるため、誤った値を引き継ぐより既定から始めるほうが安全） |
| 条件 | mode が production 以外で、v2 の `workspaces.json` が未作成、かつ marker が無いときだけ |

- v1 の envelope は `{"version":1,"workspaces":[…]}`。v2 の `WorkspaceStore` が読む形式と同じなので、
  読み取りは既存の `json_file::read_versioned` を使える。ただし **v1 の file を v2 の store 経由で
  開かない**（lock を取ると v1 と競合しうる）。read-only の一度読みに閉じる。
- v1 の path 解決は「mode を適用する前の base」＝ `DataHome::base()` を使う。production では
  base と selected が同一なので seed は行わない（条件で除外済み）。
- 実 IO は port として注入し、seed 判定は fake で全分岐をテストする。

issue / memory は既に v1 と共有しているので seed 不要である
（[16. v1 / v2 の共存](../../document/proposals/16-v1-v2-coexistence.md#領域ごとの共有分離の実態)）。

## テスト

`cargo test -p usagi-core`: v1 file あり / なし / 壊れている、marker あり（再実行しない）、
v2 の一覧が既にある（seed しない）、production mode（seed しない）、read で失敗しても起動を止めないこと。
seed 後に v1 の file が**変更されていない**ことも assert する。
