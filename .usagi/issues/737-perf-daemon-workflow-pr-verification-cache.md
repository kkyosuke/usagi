---
number: 737
title: perf(daemon): workflow の PR 検証をキャッシュしてバックオフする
status: todo
priority: high
labels: [v2, daemon, workflow, pr]
dependson: []
related: [736]
created_at: 2026-09-12T00:00:00+00:00
updated_at: 2026-09-12T00:00:00+00:00
---

## 問題

`Verifying` / `Ready` の間、workflow snapshot を取得するたびに `verify_pr` が `gh pr view`
（timeout 5 秒）を実行する。TUI の polling は「前の応答が返ったら次」という間隔なので、実質連続で
`gh` プロセスを起動し続ける。GitHub の secondary rate limit とプロセス生成コストの両方が効く。

`pr_inventory` は durable store から hydrate するキャッシュを持つのに、`verify_pr` の `gh` 直呼びだけが
非対称にキャッシュ無しになっている。

## 方針

- 検証結果を `(session, head_sha)` 単位で TTL キャッシュする。TTL 内の再取得はキャッシュを返す。
- `Waiting for successful PR checks` のような「まだ待ち」の理由では指数バックオフする（上限あり）。
- `head_sha` が変わった場合と、`Ready` から外れる遷移ではキャッシュを破棄する。
- git の HEAD / clean 判定はローカルで安価なので、TTL の対象は `gh` 呼び出しに限定してよい。
- 検証の TOCTOU fence（gh 呼び出し後の HEAD 再確認）は維持する。

## 受入条件

- [ ] 同一 `(session, head_sha)` への連続した検証で `gh` 呼び出しが TTL ごとに 1 回になる。
- [ ] 「待ち」理由が続く間はバックオフし、上限で頭打ちになる。
- [ ] HEAD 変更・phase 離脱でキャッシュを破棄し、古い判定を再利用しない。
- [ ] fake の `GhProcessPort` で呼び出し回数を数える test がある。
