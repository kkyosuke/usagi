---
number: 717
title: fix(daemon): retire sweep の lock 順序を直し、閉じた workspace の記録を守る
status: done
priority: high
labels: [v2, daemon, correctness, workspace]
dependson: []
related: [710, 712, 713]
created_at: 2026-08-24T00:00:58.167704+00:00
updated_at: 2026-08-26T02:19:33.596536+00:00
---

#708–#713 の実装レビューで見つけた欠陥 3 件。いずれもマージ済みコードの不具合。

## D1（重大）retire sweep と scope 解決で lock 順序が逆 → daemon がハングする

`TenantRegistry::retire_idle` は registry の lock を握ったまま `WorkspaceActivity::has_work` を呼び、その実装が PTY / Agent runtime の lock を取る。一方 request 経路は逆順で進む。

```
retire worker:  registry lock ──→ terminal / agent lock
request:        terminal lock ──→ (scope 解決) ──→ registry lock
```

`SharedTerminal::handle` は terminal runtime の lock を取ってから `GenericTerminalRuntime::handle` を呼び、その中の scope resolver が registry を引く（Agent も `agent.lock()` → `launch_after_readiness` → `resolve_available_scope` → registry）。30 秒ごとの sweep と launch が重なると両者が永久に待ち合う。**症状は「daemon は生きているのに全 workspace の全 client が無反応」**で、custody 監視は別スレッドなので自主終了もせず、復旧は再起動しかない。

**方針**: 観測を lock の外へ出す。「候補の選定（lock 内。tenant handle は clone しない — 参照数が判定材料そのものなので）→ 観測（lock 外）→ 確定（lock 内で参照数と fence を再確認）」の 3 相にする。`WorkspaceActivity::has_work` の引数を `&Tenant` から `(WorkspaceId, &R)` へ変えて、観測が tenant への参照を持てないようにする。

## D2（中）retire した workspace の PR inventory が消される

#713 で PR inventory の prune を「全 tenant の session の和」にした。当時は tenant が増えるだけだったので安全だったが、#712 の retire で **和が縮む**ようになった。`PrInventory` は `BTreeMap<SessionId, _>` で workspace を持たないため、`retain_sessions` が retire された workspace の記録を削除する。失われるのは last-known title/state だけでなく**利用者が付けた pin / dismiss** も含む。

## D4（中）restart 後の Agent 再照合が他 workspace の Agent を閉じる

同じ根で、startup の `reconcile_removed_session_agents` は「adopt 済み workspace の session」と照合する。起動直後に adopt されているのは 1 つだけなので、durable shard に残る**他 workspace の Agent 記録がすべて「session が消えた」と判定される**。

**D2 / D4 の方針**: daemon 全体で 1 つしかない registry の prune は「この data directory が知っている workspace すべて」で判定する。各 state subtree の lifecycle document を読んで session の和を作り、1 つでも読めなければ prune しない（部分的な view での prune は、まさに防ぎたい削除になる）。

## D3（小）`bound` client は retire 済み workspace で拒否される

`bound` 申告は保持中 workspace の最長一致しか解決しないため、retire された workspace（あるいは TUI で開く前の workspace）で動く CLI / MCP が拒否される。retire が入ったことで「10 分席を離れたら CLI が使えなくなる」という形で日常的に踏む。

**方針**: 保持していない場合は、この data directory が**かつて開いた** workspace（state subtree の `root.json`）の最長一致を探して adopt する。一度も開いていない directory は従来どおり拒否する（directory だけでは workspace root を名指せないので claim しない）。

## 受入条件

- registry の lock を持たずに観測することを、観測中に lock が空いていることを確認する回帰テストで固定する（欠陥版で落ちることを確認する）。
- 閉じた workspace の session が「知っている session」に含まれること、subtree が読めないときは prune しないことを test で固定する。
- retire 済み workspace の `bound` client が adopt し直されて admit されること、一度も開いていない directory は拒否されることを test で固定する。
- カバレッジ 100% を維持する。
