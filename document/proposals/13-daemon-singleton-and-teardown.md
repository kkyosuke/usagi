# 13. daemon singleton と session teardown

> [設計提案一覧](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [terminal VT snapshot](12-terminal-vt-snapshot.md)

本書は、v2 daemon の**単一インスタンス保証**と**session teardown の実行位置**についての未実装設計である。
実地調査で 3 つの独立した欠陥を確認し、実装 issue
[#540](../../.usagi/issues/540-fix-daemon-daemon-serve-self-shutdown-test-fixture-workspace.md) /
[#542](../../.usagi/issues/542-fix-daemon-fence-workspace-mode-home.md) /
[#543](../../.usagi/issues/543-fix-daemon-session-remove-worktree-teardown-worker-ipc.md)
に分割した。本書が採用機構・却下した代替案・fence の単位・crash 時の再開契約の設計判断の正本であり、実装が確定したら
該当部分を [5. daemon](../05-daemon.md)（process lifecycle / data directory / durable operation）と
[7. MCP サーバ](../07-mcp.md)（`session_remove` の受理契約）へ畳み込む。

## 目次

- [観測された欠陥](#観測された欠陥)
- [3 つの欠陥の関係](#3-つの欠陥の関係)
- [設計 1: custody 喪失による self-shutdown](#設計-1-custody-喪失による-self-shutdown)
- [設計 2: fence の単位を workspace へ広げる](#設計-2-fence-の単位を-workspace-へ広げる)
- [設計 3: teardown worker と resume 契約](#設計-3-teardown-worker-と-resume-契約)
- [却下した代替案](#却下した代替案)
- [test 戦略](#test-戦略)
- [docs 畳み込み先](#docs-畳み込み先)

## 観測された欠陥

調査の起点は「coordinator 環境に `usagi daemon serve` が最大 20 プロセス残留していた」という観測である。
現地で生存していた 1 件を直接観測した結果、当初の仮説（launchd と手動起動で runtime mode が食い違い、
別々の `daemon.lock` を取って共存している）とは**異なる原因**が確定した。

```text
PID 25529  PPID 1  ELAPSED 01:48:04
  .../.usagi/sessions/issue-534/target/llvm-cov-target/debug/usagi daemon serve
  USAGI_HOME=/tmp/usagi-sw0yST      ← test の tempdir。すでに削除済み
  LLVM_PROFILE_FILE=.../llvm-cov-target/issue-534-%p-%m.profraw
  cwd = .../.usagi/sessions/issue-534   ← 開発者の live session worktree
  fd 3u = /private/tmp/usagi-sw0yST/local/daemon/daemon.lock
```

残留プロセスは **coverage 実行中の integration test が起動した daemon** であり、runtime mode はいずれも
既定の `local` である。すなわち残留は mode のばらつきではなく、次の 3 欠陥の合成で起きている。

| # | 欠陥 | 実装上の所在 | 状態 |
|---|---|---|---|
| 1 | daemon に「権威を失ったら終了する」自衛が無い | `SignalShutdown::wait` の shutdown 条件は signal と IPC flag だけ | 実測で確認（残留 20 プロセス） |
| 2 | fence の単位が mode 別 data directory であり、daemon が所有する workspace と一致しない | `FileInstanceLock` は `<data_dir>/daemon/daemon.lock`、権威は `current_dir()` 由来の repo root | コード調査で確定（潜在） |
| 3 | session teardown が IPC request handler 内で同期実行される | `perform_remove` の 3 段が `usagi-ipc-client` thread 上で直列 | コード調査で確定 |

欠陥 1 は、同じクラスの不具合として
[#171](../../.usagi/issues/171-fix-daemon-usagi-daemon-serve-teardown-data-dir-self-shutdown.md)（`done`）が
過去に修正している。当時も ppid=1 の孤児が 30 プロセス残留し、恒久対策として「自分の data dir が消えたら
終了する」自衛を要求していた。v2 daemon にはその自衛が無く、同じ原因で再発している。

## 3 つの欠陥の関係

3 つは独立に修正できるが、症状としては連鎖する。

```text
[欠陥 1] test が起動した daemon が reap されず残留
   └─ cwd が session worktree の内側（test が fixture workspace を指定していない）
        └─ その session の git worktree remove / remove_dir_all が失敗・停滞
             └─ [欠陥 3] remove は IPC handler 内で同期実行されるので
                  呼び出した client の接続が deadline を超えて timeout する

[欠陥 2] は日常運用では未発火（出荷バイナリは daemon を持たない v1 コード）。
   v2 出荷時に mode 切り替え運用（task run / dev / prd）で確実に踏む。
```

実装順序は **#540 → #542**（#542 の test は #540 が入れる fixture workspace helper を前提にする）、
**#543 は独立**である。ただし #543 の受入条件のうち「巨大 `target/` の削除が完了する」は、
残留 daemon が worktree を握らないこと、すなわち #540 に実質的に依存する。

## 設計 1: custody 喪失による self-shutdown

**採用する終了条件は「custody（権威）の喪失」である。** daemon は次の 2 つの invariant を周期的に検証し、
どちらかが崩れたら graceful shutdown を要求する。

| invariant | 検証内容 | 崩れた意味 |
|---|---|---|
| lock custody | 保持中の lock fd の `(dev, ino)` と、lock path を `stat` した結果が一致する | path が消えた／別 inode に置き換わった。この process はもうその data directory の singleton ではない |
| record custody | `daemon.json` が今もこの pid と OS の process-start identity を記録している | 権威が retire された、または別 owner に置き換わった |

- 検証語彙は既存の `verify_private_lock_path` と exact process-owner record（[5. daemon の daemon data
  directory](../05-daemon.md#daemon-data-directory) が正本）をそのまま再利用する。新しい identity 概念を導入しない。
- 周期は 1 秒程度の tick とし、既存の PR refresh worker と同型の worker から回す。
- 喪失時は既存の `shutdown: AtomicBool` を立てる。SIGTERM と同じ経路を通るため、endpoint retire と cleanup の
  契約は変わらない。data directory がすでに消えている場合の cleanup は **no-op として成功**しなければならない
  （block も panic もしない）。
- 判定は注入した port に対する純関数として usecase 層に置く。実 `stat` / `fstat` の薄いラッパだけが合成ルート側の
  real IO であり、`#[coverage(off)]` を使う場合の許可理由は `real_io`（[6. 開発規約の `coverage(off)`
  例外](../06-conventions.md#coverageoff-例外)）である。

`process_group(0)` による detached 起動（`ServeLauncher`）は**維持する**。前景の hangup や launcher の終了で
daemon-owned PTY を失わせないための設計であり、正しい。連動して死ぬべきなのは「親が生きているか」ではなく
「自分がまだ権威か」である。

## 設計 2: fence の単位を workspace へ広げる

現行の fence と権威は単位が食い違っている。

| | 単位 | 実装 |
|---|---|---|
| fence | mode 別 data directory | `<data_dir>/daemon/daemon.lock` を `flock` |
| 権威 | canonical workspace root | `current_dir()` を repo root として `SessionRuntime` に渡し、`<repo>/.usagi/sessions/<name>` の worktree と `usagi/<name>` branch を所有する |

`data_dir()` は `$USAGI_HOME`（無ければ `~/.usagi`）に mode 別の子 directory を足すため、**同一 workspace に対して
mode を変えるだけで別の lock file になる**。

```text
cwd = /repo,  USAGI_HOME=/H,  USAGI_RUNTIME_MODE=local       → /H/local/daemon/daemon.lock
cwd = /repo,  USAGI_HOME=/H,  USAGI_RUNTIME_MODE=production  → /H/daemon/daemon.lock
  ⇒ 2 daemon がともに fence を通り、ともに /repo の worktree と branch を所有する
```

lifecycle state は `<data_dir>/daemon/sessions.json` にあり、共有される物理資源は `<repo>/.usagi/sessions/*` の
git worktree である。したがって**独立した 2 つの durable state が同一の worktree 集合を権威として書き換える**。

**採用: workspace 単位の lock を追加し、data directory 単位の lock は維持する。**

| lock | path | 守る対象 |
|---|---|---|
| workspace lock（新設） | `<canonical repo root>/.usagi/daemon.lock` | workspace の所有権（git worktree・branch・session 名） |
| data dir lock（現行） | `<data_dir>/daemon/daemon.lock` | mode 別 data directory の record・locator・socket・durable state |

- workspace lock は **mode の子 directory の下に置かない**。これにより `local` / `dev` / `production` と
  `$USAGI_HOME` の表記差がすべて同一 file へ収束する。
- `flock` は inode に対する排他なので、path の綴り違い・symlink・macOS の `/tmp` → `/private/tmp` firmlink では
  回避できない。canonical 化が必要なのは「同じ repo を別 path 表記で開いたときに同じ path を選ぶ」ためである。
- 取得順序は **workspace lock → data dir lock** に固定する（順序固定なので deadlock しない）。両方とも endpoint 公開の
  ready hook より前に取得する。
- lock node の secure create / reopen 契約（`O_NOFOLLOW | O_CLOEXEC`、`0600`、regular / owner / `nlink == 1` 検証）は
  既存 4 node と同一のものを再利用する。
- `<repo>/.usagi/daemon.lock` は ignore rules に追加し、git 追跡下へ入れない
  （[5. daemon の session tree と ignore rules](../05-daemon.md#session-tree-と-ignore-rules)）。

**明文化する semantics**: 「1 machine × 1 canonical workspace root に daemon は 1 つ」。mode split が分離するのは
**data** であり、**workspace の所有権ではない**。git worktree は共有された物理状態なので、mode を分けても分離できない。

付随する gap として、IPC handshake は client の workspace root を検証していない。`data_dir` を共有する
workspace B の client が workspace A に束縛された daemon へ接続すると、A の session 一覧と scope を受け取る。
fence を workspace 単位にしても、この誤接続経路は fence では閉じないため handshake 側の検証として #542 に含める。

## 設計 3: teardown worker と resume 契約

`perform_remove` は [#1270](https://github.com/KKyosuke/usagi/pull/1270) で 3 段に分割され、重い削除の間は
session lock を解放している。しかし 3 段は 1 本の IPC request handler の中で直列に走るため、**呼び出した client の
接続が削除完了まで応答を受け取れない**。client の deadline budget は TUI 2,000ms / CLI 10,000ms / MCP 30,000ms で、
coverage 実行後の `target/llvm-cov-target` は数 GB あるため、削除は分オーダーになり必ず timeout する。

さらに daemon 起動時の `reconcile()` は `Deleting` を `Failed` へ落とす。`DeletePlan` は durable に残っているのに
削除は再開されず、半分消えた worktree tree と session 名を所有し続ける record が残る。v1 で `git_teardown` 中断が
次の手動 remove まで詰まった病理と同型である。

**採用: 即時 accept + daemon 所有の teardown worker。**

```text
client ── session_remove ──▶ IPC handler
                              begin_remove（session lock 下・Deleting へ遷移・DeletePlan を durable に記録）
                            ◀── accepted { operation_id, deleting } を即時返却
                                    │
                        teardown worker（1 thread・直列 drain）
                              remove_session_tree（nested worktree → remove_dir_all）
                              finish_remove（session lock を短時間だけ再取得して確定）
                                    │
client ── session_list ─────▶ deleting 行 → 完了で消滅（失敗なら Failed + 原因）
```

- reply は既存の `SessionReply { operation_id, revision, body }` で表現できるため **wire schema の変更は不要**である。
- **queue は新設しない**。`lifecycle == Deleting` かつ `delete_plan` を持つ record が、そのまま未完了 teardown の
  集合である。追加の永続 file を持たないので、queue と durable state が乖離しない。
- worker は 1 本・直列とする。N 件の削除が同時に I/O を飽和させない。queue 深さは session 数で自然に有界である。
- **`reconcile()` の `Deleting` を「`Failed` へ落とす」から「worker へ再投入する（resume）」へ変える**。
  `remove_session_tree` は `NotFound → Ok` で冪等なので、途中まで削除された tree に対して安全に再実行できる。
  これが本設計の中心的な correctness 改善である。`Creating` / `Initializing` の `Failed` 化は変えない
  （create は effect を巻き戻せないため）。
- client 表示は既存の投影で足りる。`snapshot()` は
  [#529](../../.usagi/issues/529-fix-daemon-failed-session-remove.md) 以降すべての durable record を `lifecycle` 付きで
  投影し、[4. daemon IPC の managed session request](../04-ipc.md#managed-session-request) も `deleting` scope への
  launch を typed refusal として定義済みである。
- 冪等性は、同一 `operation_id` の再送は既存の journal replay、`Deleting` な session への**新しい** `operation_id` は
  重複投入せず進行中 operation を返す、で閉じる。

## 却下した代替案

| 代替案 | 却下理由 |
|---|---|
| daemon の idle timeout（client 0 で一定時間後に終了） | 正当な daemon は client が 0 でも live PTY と supervisor scheduler を所有する。idle は終了根拠にならない。custody 喪失は「この process はもう誰の権威でもない」を意味する精密な signal であり、policy tuning も不要である |
| 親プロセス死亡検知（`getppid` 監視 / `PR_SET_PDEATHSIG`） | detached 起動（`process_group(0)`）は前景 hangup で PTY を失わせないための正しい設計であり、親の生死に daemon の生死を結び直すのは退行である。macOS には `PDEATHSIG` 相当も無い |
| 起動経路（launchd plist / MCP 注入 / shell）の env 解決を統一して fence の分裂を防ぐ | plist と MCP 注入は統一できるが、利用者自身の shell（`USAGI_RUNTIME_MODE=production usagi ...`、`task prd`）は強制できない。env の合意は運用規約でしか守れず invariant にならない。lock は表記に依らない invariant なので、fence の正しい実装は lock 側である |
| workspace lock だけにして data dir lock を撤去する | data directory 単位の record / locator / socket / durable state は依然その単位で排他が必要である。2 段とも残すのが正しい |
| teardown を request ごとに thread へ投げる（worker を持たない） | 削除の同時実行が I/O を飽和させ、crash 後の再開も表現できない。durable state から導出する単一 worker が、有界性と resume の両方を同時に満たす |
| teardown 用の永続 queue file を新設する | `Deleting` + `DeletePlan` が既に durable な未完了集合であり、二重管理は乖離の原因になる |

## test 戦略

| 対象 | test の置き場所と形 |
|---|---|
| custody 判定 | 注入した fake port に対する usecase の unit test（lock inode 一致 / 不一致 / path 消失、record 一致 / 消失 / 別 owner） |
| self-shutdown の実挙動 | 実プロセスを起動し、data directory（または `daemon.lock`）を削除して 1 tick 周期内に終了することを固定する integration test |
| test 起動の隔離 | `tests/support/` の共有 helper 経由でのみ daemon を起こし、helper が「daemon の workspace root が fixture である」ことを assert する（開発者の worktree への束縛の回帰テスト） |
| workspace fence | 同一 workspace × mode 差 / `$USAGI_HOME` 差 / path 綴り差（末尾スラッシュ・symlink 経由）の全組合せで 2 つ目が typed refusal になること。別 workspace は並行起動できること（test の並列実行を壊さない） |
| teardown worker | fake の削除 port で「即時 accept → worker 完了 → 確定」の順序と、重複投入が effect を二重実行しないことを固定する |
| teardown resume | `Deleting` を durable に残した状態から runtime を再 open し、`Failed` にならず worker へ再投入されることを固定する |

いずれも port 注入で fake 可能にし、実 IO の薄いラッパだけを合成ルートへ置く。カバレッジ 100% は維持し、
`#[coverage(off)]` を使う場合の許可理由は `real_io` に限る。

## docs 畳み込み先

| 実装 | 畳み込み先 |
|---|---|
| custody 喪失による self-shutdown | [5. daemon の daemon process lifecycle](../05-daemon.md#daemon-process-lifecycle) |
| workspace × data dir の 2 段 fence | [5. daemon の daemon process lifecycle](../05-daemon.md#daemon-process-lifecycle) / [daemon data directory](../05-daemon.md#daemon-data-directory) |
| `<repo>/.usagi/daemon.lock` の ignore rules | [5. daemon の session tree と ignore rules](../05-daemon.md#session-tree-と-ignore-rules) |
| teardown worker と resume 契約 | [5. daemon の durable operation](../05-daemon.md#durable-operation) |
| `session_remove` が受理を返す契約 | [7. MCP サーバ](../07-mcp.md) |
