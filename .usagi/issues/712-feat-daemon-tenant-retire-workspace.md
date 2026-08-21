---
number: 712
title: feat(daemon): 遊休 tenant を retire して workspace の所有権を返す
status: todo
priority: low
labels: [v2, daemon, lifecycle, workspace]
dependson: [711]
related: []
created_at: 2026-08-20T23:34:30.227260+00:00
updated_at: 2026-08-21T00:11:04.477210+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 5。#711 の後の運用で必要性を測ってから着手する。

## 問題・根拠

daemon が workspace を on-demand で adopt するようになると、一度開いた workspace の fence と `SessionRuntime` を
daemon が保持し続ける。その workspace を別の daemon（別 mode の開発用など）で開きたいとき、daemon 全体を
止めるしかない。tenant 数の上限にも当たりやすくなる。

## 方針

- live runtime も未完了 durable operation も無く、参照する client も無い tenant を、一定時間後に graceful に retire して fence と `SessionRuntime` を解放する。
- `usagi daemon retire <path>` で明示的に 1 tenant だけ解放できるようにする。live runtime がある tenant の retire は `stop` と同じく `--force` を要求する。
- 判定は観測できる状態だけで行い、固定 sleep で代用しない。retire した tenant は次の接続で再び adopt される。

## 受入条件

- 遊休条件の各要素（live runtime あり / client 参照あり / 未完了 operation あり）で retire しないことと、すべて無いときに retire することを fake 時計・fake 観測の test で固定する。
- retire 後に workspace fence が解放され、別 process が取得できる。retire した tenant への再接続が adopt をやり直して成功する。
- `daemon retire` が指定した tenant だけを解放し、他 tenant の live runtime に影響しない。
- カバレッジ 100% を維持する。
