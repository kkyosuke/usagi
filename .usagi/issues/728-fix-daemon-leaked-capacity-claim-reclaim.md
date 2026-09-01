---
number: 728
title: "fix(daemon): 説明されない capacity claim を回収して Agent 起動不能を解消する"
status: done
priority: high
labels: [v2, daemon, resources, allocator, clean, correctness]
dependson: []
related: [518, 526, 209]
created_at: 2026-08-31T02:10:00+00:00
updated_at: 2026-08-31T02:10:00+00:00
---

## 問題

どの session でも Agent を起動できなくなる状態が発生する。原因は global resource allocator の
capacity pool 枯渇である。

実際に観測した data directory では、`AGENT_RUNTIME_LIMIT`（16）に対して `live` な agent claim が
ちょうど 16 件あり、そのうち 12 件は **既に消滅した 2 つの daemon generation** が所有していた。
生きている runtime は 5 件程度しかない。

capacity の解放は証拠に基づいており、経路は次の 2 つしかない。

- definite failure
- owner generation が publish した exit を active consumer が apply する

どちらも「claim に対応する owner shard の entry があること」を前提にしている。retired generation の
shard entry が失われた claim は、

- `super::drain` が必要とする outbox event を、消滅した owner はもう publish できない
- `release_unowned` が必要とする record が既に無い

ため、どちらの経路からも到達できず pool の slot を永久に占有する。hydrate の既存回収は shard entry を
起点に走査するので、entry が無い claim は最初から視界に入らない。

さらに 2 つの取りこぼしが同じ症状を悪化させる。

- session 削除は Agent だけを close し、その session の generic terminal を放置する。terminal は
  worktree 内に cwd を持つ child と capacity claim を握るため、worktree 撤去が使用中で失敗し、
  claim も daemon の生存中ずっと pool に残る。
- 既に leak した claim を daemon の外から回収する手段が無い。`usagi clean` は workspace・data・
  worktree・branch・process しか見ない。

## 修正方針

- active generation の hydrate で、**どの retained shard も説明しない foreign claim** を解放する。
  `ambiguous` final は child が存在し得るので対象外にする。回収件数を startup log に残す。
- `GenericTerminalCoordinator` / `TerminalRuntime` に `close_session` を追加し、session teardown で
  Agent と同じ順序（worktree 撤去より前）に terminate/reap する。
- `usagi clean` に capacity claim の候補を追加する。daemon 稼働中の可能性があるため、hydrate より
  条件を 1 つ厳しくし、`generations.json` が owner を載せていないことを追加で要求する。
- 仕様（05-daemon / 01-overview / README）を同じ変更で更新する。

## 受入条件

- [x] shard が説明しない retired generation の live claim が hydrate で released になる。
- [x] 回収で空いた slot に、直前まで拒否されていた launch が admit される。
- [x] hydrate 中の generation 自身の claim と `ambiguous` final は解放されない。
- [x] `Draining` generation は回収しない。
- [x] session 削除がその session の generic terminal を terminate/reap し、workspace-root terminal は残す。
- [x] reap 失敗は record を残して typed error を返す。
- [x] `usagi clean` が「unbacked かつ registry 未登録」の claim だけを候補にする。registry を読めない
      場合は候補 0 件になる。
- [x] risk-based gate と PR CI の full test / coverage が green になる。
