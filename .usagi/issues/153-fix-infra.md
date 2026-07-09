---
number: 153
title: fix(infra): 書き込み拒否環境でもストアロックを取得できるようにする
status: done
priority: medium
labels: [fix]
dependson: []
related: []
created_at: 2026-07-09T10:17:16.662537+00:00
updated_at: 2026-07-09T10:20:54.267778+00:00
---

## 背景

`StoreLock::acquire_with_timeout` は `.usagi/.lock` などのストア助言ロックを `create + read + write` で開いてから `fs2::FileExt::try_lock_exclusive()` を呼んでいる。本番の seatbelt サンドボックスではロックファイルへの write オープンだけが拒否され、既存ロックファイルを read できるにもかかわらず `Operation not permitted (os error 1)` で失敗した。

## 目的

ロックファイルが既存で write オープンだけ拒否される環境でも、read-only fd で助言ロックを取得できるようにする。

## 変更方針

- まず従来どおり `create + read + write` でロックファイルを開く。
- `PermissionDenied` で失敗し、かつロックファイルが既存なら `read(true)` のみで開き直す。
- 取得した fd で従来どおり `try_lock_exclusive()` のポーリングを行う。
- 初回作成時に write が拒否されるケース、タイムアウト、wedged holder のエラー化は従来どおり維持する。

## 非目標

サンドボックスが `.usagi/` 配下の全書き込みを拒否している場合、本改修後も後続の `state.json` などの書き込みで失敗する。その根本対処は codex/fugu の sandbox `writable_roots` に `.usagi` を追加するなど環境側設定であり、本 issue のスコープ外。本改修は「read は許可されるが lock の write オープンだけ拒否される」ケースを救う堅牢化である。
