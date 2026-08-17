---
number: 683
title: test(daemon): rollover E2E が transient な socket 断を product 失敗として扱う
status: done
priority: high
labels: [v2, test, ci, daemon, lifecycle]
dependson: []
related: [574, 682]
created_at: 2026-08-14T00:23:20.764789+00:00
updated_at: 2026-08-14T00:33:41.113391+00:00
---

## 問題・影響

`tests/agent_ipc_e2e.rs::root_restart_rolls_over_two_real_pty_children_without_provider_resume` が
CI で落ちた（PR #1487 / run 31752177710。tui presentation しか触っていない PR）。`full-test` は
共有 gate なので、無関係な変更の PR が落ちる。

```text
thread 'root_restart_rolls_over_two_real_pty_children_without_provider_resume'
panicked at tests/agent_ipc_e2e.rs:1736:27:
the successor refused a launch: Unavailable("daemon closed while awaiting response")
```

ローカルの full suite では別の箇所でも観測されている。

```text
panicked at tests/agent_ipc_e2e.rs:253:
Unix IPC handshake succeeds: Unavailable("daemon closed before a server hello")
```

どちらの `Unavailable` も、**framed な拒否ではなく socket が閉じただけ**である
（`usagi_core::usecase::client` で、hello 待ちの EOF と応答待ちの EOF にそれぞれ対応する）。

## 切り分け

### これは CPU 競合ではない

失敗した CI run の `agent_ipc_e2e` は 11 件で **8.32 秒**しかかかっておらず、この test は
20 秒 deadline を使い切らずに**最初の 1 回**で落ちている。CI runner に競合相手はいない
（`full-test` と `coverage` は別 job・別 runner）。[#682](682-test-e2e.md) の CPU 競合とは別件である。

### テスト側が transient を product 失敗として扱っている

2 か所とも、**bounded な待ちループの中**で起きている。

| 箇所 | 待っているもの | retry していた error | していなかった error |
|---|---|---|---|
| `client()`（`:253`） | daemon が socket を publish するまで | `connect_current` の失敗のみ | handshake の transport 断（`.expect` で即 panic） |
| successor launch loop（`:1736`） | 後継世代が control gate を開けるまで | `GenerationRolledOver` のみ | transport 断（`panic!` で即失敗） |

一方 production の client は、この 2 つをどちらも「まだ何も dispatch されていない／接続が壊れた」として
新しい接続で retry する（`PolicyClient` は `ClientError::is_transport_failure` を retry 対象にする）。
つまりテストは production より狭い条件しか許しておらず、daemon が endpoint 公開中・listener retire 中・
pre-handshake 上限で accept 直後に close したといった**正常な過渡状態**を失敗に変換していた。

successor launch の再送が安全であることは、この test 自身の作りから言える。`successor_intent` は
loop の外で 1 度だけ作られ、固定の producer `OperationId` を持つ。再送は daemon 側で同じ durable
operation に収束するので、二重 spawn にはならない。仮になったとしても、直後の
`shell_spawns` が 1 行であることの assert が落ちる。

### なぜ「どちらか」を判定できなかったのか

daemon の stderr は production と同じく `/dev/null` へ捨てられているため、失敗メッセージからは
「transient で閉じたのか、daemon の thread が panic して閉じたのか」が読めない。実際には daemon は
process 全体の panic hook で**全 thread の panic を `<data dir>/logs/` に記録している**。テストがこれを
読んでいなかったので、証拠があるのに使われていなかった。

なお本セッションでは、fix 前のコードで full suite 3 回・`agent_ipc_e2e` binary 14 回・当該 test 単独
40 回超（人工 CPU 負荷つき）を回したが再現しなかった。**この issue は「再現させて原因を 1 つに特定する」
のではなく、「transient を吸収し、product 失敗なら証拠つきで即落ちる」形に変えることで決着させる。**

## 対象責務

- `client()` は handshake の transport 断を readiness deadline 内で retry する。framed な拒否
  （workspace 不一致など）は従来どおり即失敗。
- successor launch loop は `GenerationRolledOver` に加えて transport 断も同じ bounded wait で retry する
  （固定 `OperationId` の再送なので冪等）。
- retry は product 失敗を**絶対に吸収しない**。
  - retry のたびに daemon の error log を見て `daemon panicked` があれば即失敗する。
  - 後継 daemon の pid が生きていることを確認し、死んでいれば deadline を待たず即失敗する。
- 失敗メッセージに daemon の error log を添え、次回の発生を無言にしない。
- 規約に「transient と product 失敗を混同しない」節を追加する。

## 非対象

- deadline を伸ばすこと、固定 sleep を足すこと。
- daemon 側 product の変更。閉じた原因が product 由来なら、上記の panic 検出と error log 添付が
  次の発生で証拠を出すので、その時点で別 issue にする。

## 受入条件

- [ ] 2 か所の待ちが、transport 断を deadline 内で retry する。
- [ ] daemon thread の panic は retry で吸収されず、panic 本文つきで即失敗する。
- [ ] 後継 daemon の死亡は deadline 満了ではなく即失敗として出る。
- [ ] rollover の契約（後継は新しい子を spawn し、G1 の子を引き継がない／各 fixture の spawn は 1 回）は弱まっていない。
- [ ] `cargo test --workspace --quiet` が複数回 green。
