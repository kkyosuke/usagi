---
number: 682
title: test: 重い E2E をプロセスを跨いで直列化する
status: done
priority: high
labels: [v2, test, ci, daemon, tui]
dependson: []
related: [574, 567]
created_at: 2026-08-14T00:22:40.236509+00:00
updated_at: 2026-08-14T00:33:38.834943+00:00
---

## 問題・影響

重い E2E（shipping binary・実 daemon・fixture provider・実 PTY）が、**別プロセスの負荷**で CPU を奪われて
timeout し、product の失敗ではないのに落ちる。落ちるのは共有 gate（`full-test` / `coverage`）なので、
無関係な変更の PR が落ちる。

観測:

| 観測 | 条件 |
|---|---|
| `tests/cli_tui_pty.rs::real_pty_cold_restart_resumes_only_the_selected_interrupted_tab_from_real_keys` と `real_pty_root_launch_keeps_the_managed_agent_tab_live` が失敗 | `cargo test --workspace --quiet` を `cargo llvm-cov` と**同時**に実行（同じチェックアウト）。binary は 131s |
| `tests/cli_tui_pty.rs::real_pty_roles_editor_ctrl_s_persists_workspace_catalog` が `timed out waiting for Leave this workspace?` で失敗 | 他セッションが同居する開発機での `cargo test --workspace --quiet`。binary は 98.43s（12 件中 11 pass） |
| 12 件すべて green | `cargo test -p usagi --test cli_tui_pty` 単独実行。64s |

いずれも「観測できるまで loop を駆動し上限で失敗させる」形の待ちが、上限側で落ちている。待ち方の問題ではなく、
**待っている間に CPU が無い**ことが原因である。

## 切り分け

当初の仮説「`cargo test --workspace` は test binary 同士を並行実行するので、in-process lock では
`agent_ipc_e2e` と `cli_tui_pty` が同時に走る」は**否定された**。

full workspace run の間 0.3 秒ごとに process table を 468 サンプル取ったところ、同時に走っている
test binary は**常に 1 個**だった（2 個以上のサンプルは 0）。cargo は test target を直列に実行する。

実際の抜け穴は「同じチェックアウトで **cargo を 2 つ**走らせたとき」である。in-process の
`LazyLock<Mutex<()>>` は同じ process の thread しか直列化しないため、`cargo test` と `cargo llvm-cov`
（`scripts/coverage.sh` 経路）が同居すると、両者の重い E2E が同時に実 daemon と実 PTY を掴む。

| 競合の出どころ | 旧 in-process lock | 必要な仕組み |
|---|---|---|
| 同じ test binary 内の別 test | 覆う | そのまま |
| 同じ cargo 実行の別 test binary | そもそも競合しない（cargo が直列実行） | 不要 |
| 同じチェックアウトの別 cargo 実行 | **覆わない** | cross-process ロック |
| 別チェックアウト・マシン上の他プロセス | 覆わない | 環境側の条件（対象外） |

規約 [重い E2E の直列化](../../document/06-conventions.md#重い-e2e-の直列化) は「1 test binary 内では直列に実行する」
としか書いておらず、保証がプロセス境界で止まることも、止まると何が起きるかも書いていない。

## 対象責務

- `tests/support/daemon.rs` に**チェックアウト単位の cross-process 排他ロック**を置き、
  `tests/agent_ipc_e2e.rs` と `tests/cli_tui_pty.rs` の両方を同じ 1 本の直列列に載せる。
- lock file は `target/` の下に置かない。`cargo test` と `cargo llvm-cov` は別の target directory を使うため、
  target 配下だと**同じ tree の 2 実行が別のロックを取り**、直列化したい当の組み合わせだけが漏れる。
  temp directory 上に、チェックアウト path の digest で名付ける。
- ロック取得は上限付きにし、超えたら lock file を名指しして失敗する（先行 process の hang を無期限停止にしない）。
- 規約を実態に合わせる。cargo の直列実行という測定結果と、プロセス境界で保証が止まること、覆えない範囲を明記する。

## 非対象

- 別チェックアウト・別ユーザーの負荷までを直列化すること（マシン全域ロックは、hang した 1 本が
  無関係なチェックアウトを全部止めるため採らない）。
- 個々の待ちの deadline を伸ばすこと。規約どおり、固定 sleep や deadline 延長では直さない。

## 受入条件

- [ ] `agent_ipc_e2e` と `cli_tui_pty` を**同時起動**しても、両者の重い区間が重ならない。
- [ ] 単独実行・`cargo test --workspace` の通常経路が従来どおり green。
- [ ] 新しい重い E2E が同じ列に乗る手順が規約に書かれている。
- [ ] 規約の「重い E2E の直列化」節が、測定した cargo の挙動と、覆う範囲・覆わない範囲を述べている。
