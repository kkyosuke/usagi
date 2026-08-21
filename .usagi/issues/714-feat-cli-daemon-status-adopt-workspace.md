---
number: 714
title: feat(cli): daemon status に adopt 済み workspace の一覧を出す
status: todo
priority: medium
labels: [v2, cli, daemon, workspace]
dependson: [710]
related: []
created_at: 2026-08-21T02:19:56.661282+00:00
updated_at: 2026-08-21T02:19:56.661282+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。#710 の実装中に切り出した残件。

## 問題・根拠

daemon が複数の workspace を adopt するようになったが、`usagi daemon status` は lifecycle record（pid と process-start identity）しか表示しない。利用者は「この daemon がどの workspace を掴んでいるか」を知る手段がない。

state subtree（`daemon/w/<digest>/root.json`）を読めば「**かつて** adopt された workspace」は分かるが、稼働中の daemon が **いま**保持している集合とは一致しない（restart 後は subtree が残ったまま tenant は空になる）。ディスクの痕跡を現在の保持として表示すると誤解を招く。

## 方針

- 稼働中の daemon へ問い合わせて、保持中の tenant（root・session 数・live runtime 数）を返す read-only な IPC を足す。申告は `unbound`（workspace resource を読まないため）。
- `usagi daemon status` はその応答を lifecycle record の下に列挙する。daemon が居ない場合は従来どおり record の状態だけを出す。
- 表示は root と要約に限る（path 以上の内部状態は出さない）。

## 受入条件

- 2 つの workspace を adopt した daemon に対して `daemon status` が両方を列挙することを結合テストで固定する。
- daemon 不在・stale record のときの出力が従来どおりであること。
- カバレッジ 100% を維持する。
