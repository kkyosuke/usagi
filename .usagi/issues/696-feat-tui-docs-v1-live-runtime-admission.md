---
number: 696
title: feat(tui,docs): v1 へ戻すときの live runtime admission と引き継がれないものの告知
status: todo
priority: medium
labels: [tui, docs]
dependson: [694]
related: [693, 695]
created_at: 2026-08-17T23:21:17.242768+00:00
updated_at: 2026-08-17T23:33:42.589279+00:00
---

設計は [document/proposals/17-v2-trial-channel.md](../../document/proposals/17-v2-trial-channel.md) の
「P4: 戻り道と告知」が正本。戻り道そのもの（Config の `Version` 行）は
[#694](694-feat-cli-tui-channel-switch-v2-config-cli-v1-v2.md) が作る。本 issue はそこで v1 を選んだときの
**admission** と **告知**を持つ。

## 1. live runtime の admission

symlink を戻しても v2 の daemon は動き続け、PTY と workspace fence を持ったままになる。
ここは新しい規則を作らず、**既存の `daemon stop` の admission をそのまま使う**。

| beta 側の状態 | Config で v1 を選んだときの動作 |
|---|---|
| daemon が停止している | symlink を差し替え、次の起動から v1 になることを伝える |
| daemon は動いているが live Agent / terminal が無い | daemon を停止してから差し替える |
| live Agent / terminal がある | **差し替えない**。件数を示して pane を閉じるか明示的に手放すことを促す（`daemon stop` が `--force` を要求するのと同じ判断） |

- 判定は daemon への通常の問い合わせで行い、lifecycle state を書き換えたり worktree を触ったりしない。
- `usagi channel use stable`（CLI）も同じ admission を通す。UI と CLI で判断が分かれないよう、
  admission は usecase に 1 つ置いて両方から呼ぶ。

## 2. 引き継がれないものの告知

戻したときに見えなくなるものがある。試用の性質上避けられないので**隠さない**。

| 試用中に v2 で作ったもの | v1 へ戻したときの見え方 |
|---|---|
| issue / memory | **そのまま見える**（共有） |
| workspace 登録 | v1 の一覧には出ない。v1 側で改めて開けばよい |
| session worktree と `usagi/<name>` branch | **v1 の一覧に出ない**。git 上には実体として残る |

session が引き継がれないのは lifecycle state が別だからで、v2 は自分の state 外の worktree を掃除しない
ため壊れはしないが、v1 から見ると一覧に出ない worktree が残る。**この告知は Config で v1 を選んだ時点で出す**。

## 3. docs

- `README.md` に試用の導線: install（`USAGI_CHANNEL=beta`）→ 使う → 戻すときは Config の `Version` 行。
  共有されるもの / されないもの / 残る制約の 3 行要約を載せる。
- `document/01-overview.md` の CLI 表に `usagi channel` を追加し、`usagi update` 行を channel 対応後の
  挙動に合わせる。
- `document/03-tui.md` の Config の項目一覧に `Version` 行を追加する（Global scope のみ）。
- 残る運用制約「**同じ workspace を v1 と v2 で同時に開かない**」を告知に含める。channel を切り替えて
  使う運用なら自然に起きないが、両方を同時に起動できる状態自体は残る（v1 は workspace fence を取らない）。
- **v1 には手を入れない**（[16 の前提](../../document/proposals/16-v1-v2-coexistence.md#前提と制約)）。

## テスト

- `cargo test -p usagi-tui`: admission の 3 分岐、告知の表示、live 件数の反映。
- `cargo test -p usagi-cli`: CLI 経路が同じ admission を通ること。
- Markdown link check（lychee）。
