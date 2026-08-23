---
number: 714
title: feat(cli): daemon status の tenant 一覧と daemon retire を tenant 向け IPC で足す
status: todo
priority: medium
labels: [v2, cli, daemon, workspace]
dependson: [710]
related: []
created_at: 2026-08-21T02:19:56.661282+00:00
updated_at: 2026-08-23T22:43:41.686272+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。#710 / #712 の実装中に切り出した残件。

## 問題・根拠

daemon が複数の workspace を adopt するようになったが、外から見る手段と、明示的に返す手段が無い。

- `usagi daemon status` は lifecycle record（pid と process-start identity）しか表示せず、「この daemon がどの workspace を掴んでいるか」が分からない。
- 遊休 workspace は 10 分で自動的に返るが（#712）、**いま返したい**とき（別 mode の daemon でその workspace を開きたい等）に待つしかない。

state subtree（`daemon/w/<digest>/root.json`）を読めば「**かつて** adopt された workspace」は分かるが、稼働中の daemon が **いま**保持している集合とは一致しない（restart 後は subtree が残ったまま tenant は空になる）。ディスクの痕跡を現在の保持として表示すると誤解を招く。

## 方針

read-only な列挙と、1 件の retire を同じ tenant 向け IPC 経路で足す（どちらも「稼働中 daemon だけが答えられる問い」なので分けない）。

- 保持中の tenant（root・session 数・live runtime 数）を返す request を足す。申告は `unbound`（workspace resource を読まないため）。
- `usagi daemon status` はその応答を lifecycle record の下に列挙する。daemon が居ない場合は従来どおり record の状態だけを出す。
- `usagi daemon retire <path>` は 1 tenant だけを解放する。live runtime を持つ tenant の retire は `stop` と同じく `--force` を要求する。起動 workspace（`serve` が fence を持つ）は retire できないことを typed に伝える。
- 表示は root と要約に限る（path 以上の内部状態は出さない）。

## 受入条件

- 2 つの workspace を adopt した daemon に対して `daemon status` が両方を列挙することを結合テストで固定する。
- `daemon retire <path>` が指定した tenant だけを解放し、他 tenant の live runtime に影響しないことを結合テストで固定する。
- daemon 不在・stale record のときの `status` 出力が従来どおりであること。
- カバレッジ 100% を維持する。
