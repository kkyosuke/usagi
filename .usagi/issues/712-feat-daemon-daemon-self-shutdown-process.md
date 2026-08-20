---
number: 712
title: feat(daemon): 遊休 daemon の self-shutdown で常駐 process を回収する
status: todo
priority: low
labels: [v2, daemon, lifecycle, workspace]
dependson: [711]
related: []
created_at: 2026-08-20T23:34:30.227260+00:00
updated_at: 2026-08-20T23:34:30.227260+00:00
---

設計は [document/proposals/17-multi-workspace-daemon.md](../../document/proposals/17-multi-workspace-daemon.md) が正本。本 issue はその段階 5（任意。#711 の後の運用で必要性を測ってから着手する）。

## 問題・根拠

workspace ごとに daemon が立つようになると、開いたことを忘れた workspace の daemon が常駐し続ける。
現在 daemon が自主終了するのは SIGINT / SIGTERM、IPC 由来の shutdown 要求、custody 喪失だけである。

## 方針

- live runtime が無く、接続中 client も無く、durable な未完了 operation も無い状態が一定時間続いた daemon を、custody 喪失と同じ graceful path で終了させて fence を返す。
- supervisor（LaunchAgent / systemd user unit）が install されている workspace は対象外にする（即座に起こし直されるため）。
- 判定は観測できる状態だけで行い、固定 sleep で代用しない。

## 受入条件

- 遊休条件の各要素（live runtime あり / client 接続あり / 未完了 operation あり）で終了しないことと、すべて無いときに終了することを fake 時計・fake 観測の test で固定する。
- 終了後に workspace fence と単一インスタンス lock が解放され、次の open が cold start で成功する。
- supervisor 配下では終了しない。
- カバレッジ 100% を維持する。
