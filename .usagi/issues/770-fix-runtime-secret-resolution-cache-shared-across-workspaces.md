---
number: 770
title: fix(runtime): secret 解決 cache を参照単位で daemon 全体に共有する
status: todo
priority: medium
labels: [runtime, env, secret, daemon]
dependson: []
related: [735]
created_at: 2026-09-25T00:00:00+09:00
updated_at: 2026-09-25T00:00:00+09:00
---

## 問題

設定 env の `op://` 解決結果は `UserEnvironment`（`src/runtime/user_env.rs`）が cache するが、key が
**workspace root** である。global settings の binding は全 workspace で同一なのに workspace ごとに別 entry に
なるため、同じ参照でも workspace の数だけ `op read` subprocess が走る（既存の
`caches_each_workspace_separately` がこの粒度を固定している）。

1Password CLI を desktop app 連携で使っている場合、この `op read` 1 回ごとに利用者の承認が要る。そのため
「workspace を開くたびに 1Password に聞かれる」状態になり、`OP_SERVICE_ACCOUNT_TOKEN` を使わない利用者では
承認回数が workspace 数に比例する。daemon は data directory ごとに 1 process で複数 workspace を adopt する
（[5. daemon#tenant registry](../../document/05-daemon.md#tenant-registry)）ので、この重複は daemon を跨いだ
制約ではなく cache の粒度だけが理由である。

cache が daemon process の memory にしかない点自体は secret を durable 化しない方針として正しく、本 issue の
対象は **同一 daemon 内での重複解決** に限る。

## 修正方針

- cache の粒度を `workspace root → 解決済み map` から `(secret reference, credential identity) → 値` へ変える。
- credential identity は `OP_SERVICE_ACCOUNT_TOKEN` そのものではなく digest（`sha2`）で保持し、異なる
  credential へ他の credential で解決した値を配らない。credential 無し（desktop app 連携）も独立した
  identity として扱う。
- workspace 合成（global + workspace の merge、[workspace が bind できない変数](../../document/09-env.md#workspace-が-bind-できない変数)の拒否、
  上限検証）は従来どおり launch ごとに行う。cache するのは `op://` の解決値だけで、平文 binding は cache しない。
- 解決に失敗した参照は cache しない。locked vault で 1 度失敗した参照が daemon の生存中ずっと未解決へ
  固定されないこと。
- 解決値は従来どおり durable state・error log・IPC wire に載せない。

## 受入条件

- 同じ `op://` 参照を持つ 2 つの workspace を連続 launch したとき、`op read` が 1 回だけ呼ばれる
  （fake resolver の呼び出し記録で検証）。
- workspace ごとに異なる平文 binding は従来どおり workspace ごとの値になる。
- 参照を編集した binding は次の launch で再解決される。
- `OP_SERVICE_ACCOUNT_TOKEN` を変更すると、以前の credential で解決した値を再利用せず再解決する。
- 解決に失敗した参照は cache されず、次の launch で再試行される。
- [9. 環境変数設定#secret の解決](../../document/09-env.md#secret-の解決) の cache 記述を参照単位の
  daemon 共有 cache に更新する。
