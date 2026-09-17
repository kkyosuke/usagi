---
number: 757
title: refactor(root): 合成ルート daemon.rs / dispatch.rs から decode・authorization・response shaping を daemon 面へ移す
status: done
priority: high
labels: [v2, refactor, root, daemon, coverage]
dependson: []
related: [758, 763]
created_at: 2026-09-16T00:00:00+00:00
updated_at: 2026-09-16T18:27:37.129429+00:00
---

## 問題

`document/02-architecture.md` は合成ルートを「面の選択だけを担う」と定義するが、`src/runtime/daemon.rs` は
25,730 行（production 11,329 行、top-level 型 95、`impl` 117、`Drop` impl 14、`Arc<Mutex<_>>` 18）あり、
直近 400 commit の変更回数 152 回で workspace 最多だった。`#[coverage(off)]` も workspace 535 件のうち 405 件（76%）が
`src/` に集中していた。

## やったこと

### 合成ルート daemon.rs の分割

インライン `mod tests`（14,400 行）を `daemon/tests.rs` へ出し、production を責務別の子 module へ分けた。

| 子 module | 行 | 内容 |
|---|---|---|
| `daemon/ipc_accept.rs` | 1,915 | Unix socket の accept ループと handshake、response 書き出し |
| `daemon/workers.rs` | 1,226 | 背景 worker 群と shutdown、orphan / retention の回収 |
| `daemon/standby.rs` | 1,194 | standby generation の IPC・custody・昇格 |
| `daemon/agent.rs` | 1,055 | Agent runtime の open / restart 復旧・tenant inventory・decision 保守 |
| `daemon/instance_lock.rs` | 752 | single-instance lock と workspace fence、custody 監視 |
| `daemon/pty.rs` | 743 | PTY の確保と所有、terminal runtime の composition |
| `daemon/broker.rs` | 569 | bootstrap broker の起動・endpoint 公開・idle 監視 |

`src/runtime/daemon.rs` は **25,730 → 4,204 行**（production 11,329 → 約 4,200 行）になった。
`tests/architecture.rs` に `daemon_composition_root_keeps_its_concerns_in_their_modules` を追加し、
代表 symbol が composition module へ戻らないことと、module 自体が 4,500 行を超えないことを固定した。
分割で呼び出し位置が変わった `daemon_tenant_control_stays_out_of_the_socket_and_lifecycle_composition_module` も
accept loop module を見る形へ更新した。

## 残件（未達の完了条件）

**`dispatch.rs` の handler を daemon 面へ移す部分は行っていない。** `src/runtime/daemon/dispatch.rs` は 4,675 行のまま、
`dispatch_agent_tool`（817 行）/ `dispatch_session_action`（586 行）などが `reason=composition` の `#[coverage(off)]` で残る。

これを完了するには、JSON decode・caller authorization・response envelope の shaping を
`crates/daemon/src/presentation/ipc` へ typed handler として移し、**移した分の unit test を新規に書く**必要がある。
coverage 100% が gate なので、移動と同時にテストを用意しないと CI が落ちる。moving と test 追加を合わせると本 PR とは
別の規模になり、また module 分割の回帰と混ざると切り分けができない。

したがって当初の完了条件のうち次の 2 つは未達である。

- [ ] `src/runtime/daemon/dispatch*.rs` に 100 行を超える `#[coverage(off)]` 関数が残らない
- [ ] `src/runtime/daemon.rs` が 3,000 行以下（4,204 行まで。残る 1,200 行は dispatch 側の移設と併せて減る）

## 完了条件（達成状況）

- [x] 合成ルートの責務別 module 分割と architecture guard（25,730 → 4,204 行）
- [x] production と tests を同居させない
- [x] `document/02-architecture.md` のディレクトリ構成に分割後の module を反映
- [ ] dispatch handler の daemon 面への移設（上記のとおり別 issue とすべき規模）
