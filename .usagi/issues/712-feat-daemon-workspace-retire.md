---
number: 712
title: feat(daemon): 遊休 workspace を retire して所有権を返す
status: done
priority: low
labels: [v2, daemon, lifecycle, workspace]
dependson: [711]
related: []
created_at: 2026-08-20T23:34:30.227260+00:00
updated_at: 2026-08-23T22:44:04.196578+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 5。

## 問題・根拠

daemon が workspace を on-demand で adopt するようになったため、一度開いた workspace の fence と lifecycle runtime を
daemon が保持し続ける。その workspace を別の daemon（別 mode の開発用など）で開きたいとき、daemon 全体を
止めるしかない。tenant 上限にも当たりやすくなる。

## 方針

30 秒ごとの sweep が、次の 4 つがすべて成り立つ workspace を返す。

| 条件 | 理由 |
|---|---|
| `serve` が fence した起動 workspace ではない | その fence は process のものなので、tenant を落としても workspace は返らない |
| registry の外に保持者が居ない | 接続中の client が次の request を送れる workspace は返さない |
| 自分の仕事が無い | 稼働中の generic terminal・Agent runtime、未完了 teardown、`available` でない session、未決着 operation のいずれも無い |
| その状態が 10 分続いた | 離れて戻るたびに fence を churn させない |

- 観測できないものは **仕事がある**として扱う（fail closed）。runtime の lock が取れない、lifecycle document が読めない場合に返すと、まだ動いている worktree を 2 人目の owner に渡すことになる。
- 判定は観測できる状態だけで行い、固定 sleep で代用しない。
- 保持者の有無は tenant identity の共有数で見る（registry だけが持つ = 誰も serve していない）。

## 受入条件

- 遊休条件の各要素（live runtime あり / 保持者あり / 未完了作業あり）で retire せず、すべて無いときに retire することを test で固定する。✅ registry の unit test（連続した遊休期間・途中で仕事が入ると計測がやり直しになること）と、合成ルートの test（実 runtime による観測、lock が読めないときの fail closed、worker が実際に返すこと）
- retire 後に workspace fence が解放され、別 process が取得できる。✅ fence guard の drop で解放される（unit test で観測）
- retire した tenant への再接続が adopt をやり直して成功する。✅ #710 の adopt 経路がそのまま使われる
- カバレッジ 100% を維持する。✅

明示的な `usagi daemon retire <path>` は #714 へ移した。稼働中 daemon にしか答えられない問い（保持中 tenant の列挙）と同じ tenant 向け IPC を必要とするため、そちらでまとめて足す。
