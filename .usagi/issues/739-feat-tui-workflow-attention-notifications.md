---
number: 739
title: feat(tui): workflow の判断待ちと PR 完了を通知する
status: done
priority: high
labels: [v2, tui, daemon, workflow, notification]
dependson: [736]
related: [738]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T09:00:00+00:00
---

## 問題

`Phase::Waiting`（判断待ち）と `Phase::Ready`（PR 準備完了）は、まさに人を呼ぶべき瞬間である。
しかし現状はどちらも desktop notification も `user_decision` も出さず、Workflow タブを開いている人
だけが気づける。usagi には両方の配管が既にある。

## 方針

- 通知は daemon の常駐 lane（#736）から出す。TUI が開いていなくても届く必要があり、daemon は利用者と
  同じ権限で動いているため desktop 通知を自分で起こせる。
- 対象は「人以外に動かせない」2 状態だけとする。判断待ちは待ち理由、PR 準備完了は検証した PR の URL を添える。
  Agent の手番を通知すると、利用者がこの通知を無視する習慣を作ってしまう。
- record に「どの状態を通知済みか」を持たせ、同じ状態に留まる間は再通知しない。復帰して再び同じ状態に
  なった場合は改めて通知する。
- 通知は best-effort とし、失敗しても run の進行に影響させない。

### user_decision を使わない理由

当初案は `Waiting` で `user_decision` を起票することだったが採らなかった。

- TUI は pending な decision をすべて desktop 通知するため、daemon 側の通知と**二重に通知される**。
- decision は「回答が誰かに配送される」ことを前提にした仕組みで、workflow の待ちには回答の consumer が
  いない。放置された decision が pending として積み上がる。
- session 横断で「自分待ちの一覧」を見たいという要求は #741（Director の Workflows ビュー）が扱う。

## 受入条件

- [x] 判断待ちへの遷移で、理由を添えた通知が 1 回だけ出る。
- [x] PR 準備完了への遷移で、PR URL を添えた通知が 1 回だけ出る。
- [x] TUI を開いていない状態でも発火する（daemon lane から出す）。
- [x] 同じ状態に留まっている間は再通知せず、復帰後に再び同じ状態になれば改めて通知する。
- [x] Agent の手番（実装中・レビュー中など）では通知しない。
- [x] `document/03-tui.md` と `document/05-daemon.md` に通知の条件と抑止規則を書く。
