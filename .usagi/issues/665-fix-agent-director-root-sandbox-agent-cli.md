---
number: 665
title: fix(agent): Director の root sandbox で agent CLI が認証できない
status: done
priority: high
labels: []
dependson: []
related: []
created_at: 2026-08-12T00:00:22.993633+00:00
updated_at: 2026-08-12T00:03:20.303577+00:00
---

## 症状

Home の Director drawer から New で agent を起動しても立ち上がらない。macOS で再現する。

## 原因

root mode の `usagi claude-sandbox` が組む `sandbox-exec` profile に、macOS の **per-user MDS cache**
（`$DARWIN_USER_CACHE_DIR/mds`）が writable root として入っていない。Keychain 検索は Module Directory
Service の cache を更新するため、ここが read-only だと

```
security: SecKeychainSearchCreateFromAttributes: A Module Directory Service error has occurred.
```

で検索そのものが失敗する。agent CLI は Keychain の credential を読めないまま file 側の古い credential へ
fallback し、`Failed to authenticate. API Error: 401 OAuth access token has been revoked.` で終了する。
profile は system 側の `/private/var/db/mds` だけを許していた。

同じ profile の `(deny file-write*)` は `/dev/null` の `O_RDWR` open も止めるため、root coordinator に
配線された read-only Git allowlist が
`fatal: could not open '/dev/null' for reading and writing` で全滅する。Linux の `bwrap` は `--dev /dev` で
新しい devtmpfs を張るので、この 2 つはどちらも macOS だけに出る（CI は ubuntu-latest なので踏まない）。

## 受入条件

- root sandbox の中で `security find-generic-password` が成功し、agent CLI が認証できる。
- root sandbox の中で read-only Git allowlist（`git --no-pager --no-optional-locks status` など）が成功する。
- cache root は daemon bootstrap が trusted environment から一度だけ確定し（`$TMPDIR` / `$HOME` と同じ扱い）、
  Agent child の環境変数は policy 解決に使わない。writable にするのは `<cache>/mds` だけとする。
- `/dev` は data 書き込みだけを許し、node の作成・削除・属性変更は deny のまま残す。
- session mode の writable root は own worktree だけ、という既存の境界を変えない。
