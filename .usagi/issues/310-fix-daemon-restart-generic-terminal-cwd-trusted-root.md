---
number: 310
title: fix(daemon): restart 後の generic terminal cwd を trusted root に統一する
status: done
priority: high
labels: [daemon]
dependson: []
related: []
created_at: 2026-07-17T11:10:49.288319+00:00
updated_at: 2026-07-17T11:19:06.911030+00:00
---

## 目的
shared daemon を別 cwd から再起動しても、復元済み managed session の trusted repository root と generic terminal の working directory を一致させる。

## 完了条件
- `sessions.json` に保存された trusted root を composition root が安全に受け渡す。
- generic terminal の `login-shell` profile が restart 時の process cwd ではなく復元済み trusted root を使用する。
- A で初期化し B から restart・terminal launch した回帰をテストする。
- daemon 正本ドキュメントを実装契約に更新する。
