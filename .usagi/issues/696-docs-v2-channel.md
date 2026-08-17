---
number: 696
title: docs: v2 試用 channel の戻り道と引き継がれないものを告知する
status: todo
priority: medium
labels: [docs]
dependson: [694]
related: [693, 695]
created_at: 2026-08-17T23:21:17.242768+00:00
updated_at: 2026-08-17T23:21:17.242768+00:00
---

設計は [document/proposals/17-v2-trial-channel.md](../../document/proposals/17-v2-trial-channel.md) の
「P4: 戻り道と告知」が正本。

## 背景

v2 の試用は `local` mode なので戻すのは安全でデータ移行も不要だが、**戻したときに見えなくなるものがある**。
これは試用の性質上避けられないので隠さずに告知する。

| 試用中に v2 で作ったもの | v1 へ戻したときの見え方 |
|---|---|
| issue / memory | **そのまま見える**（共有） |
| workspace 登録 | v1 の一覧には出ない。v1 側で改めて開けばよい |
| session worktree と `usagi/<name>` branch | **v1 の一覧に出ない**。git 上には実体として残る |

session が引き継がれないのは lifecycle state が別だからで、v2 は自分の state 外の worktree を掃除しない
ため壊れはしないが、v1 から見ると一覧に出ない worktree が残る。

## やること

- `usagi-channel use stable` は、beta 側に live session がある場合に**その数を示して確認を求める**。
  判定は beta channel の lifecycle state を read-only で読む（session を停止させたり worktree を触ったりしない）。
- installer の完了メッセージに、現在の channel と切替方法を出す。
- `README.md` に試用の導線を書く: install（beta channel）→ `usagi-channel use beta` → 戻すときは
  `usagi-channel use stable`。共有されるもの / されないもの / 残る制約の 3 行要約を載せる。
- `document/01-overview.md` の CLI 表と `usagi update` 行を channel 対応後の挙動に合わせる。
- **v1 には手を入れない**（[16 の前提](../../document/proposals/16-v1-v2-coexistence.md#前提と制約)）。
  導線を v1 の Welcome / Config に置かないのは意図した判断であり、理由は proposal 17 の却下表にある。
- 残る運用制約「**同じ workspace を v1 と v2 で同時に開かない**」を告知に含める。channel を切り替えて
  使う運用なら自然に起きないが、両方を同時に起動できる状態自体は残る（v1 は workspace fence を取らない）。

## テスト・確認方法

- Markdown link check（lychee）。
- `usagi-channel use stable` の live session 確認は script fixture test で固定する
  （live session あり / なし / lifecycle state が読めない）。
