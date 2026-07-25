---
number: 542
title: fix(daemon): 単一インスタンス fence を workspace 単位にし、mode/HOME の表記差で二重起動できないようにする
status: done
priority: high
labels: [v2, daemon, lifecycle, correctness]
dependson: [540]
related: []
created_at: 2026-07-24T22:39:19.054361+00:00
updated_at: 2026-07-25T02:32:01.353130+00:00
---

## 問題・根拠（コード調査で確定）

daemon の単一インスタンス fence が守っている単位と、daemon が実際に所有している資源の単位が食い違っている。

| | 単位 | 実装 |
|---|---|---|
| fence（守っている単位） | mode 別 data directory | `FileInstanceLock` が `<data_dir>/daemon/daemon.lock` を `flock` する（`src/runtime/daemon.rs` の `run_inner`） |
| 権威（所有している資源） | canonical workspace root | `spawn_ipc_server` が `std::env::current_dir()` を repo root として `SessionRuntime::open` に渡す。以後 `<repo>/.usagi/sessions/<name>` の worktree と `usagi/<name>` branch を所有する |

`data_dir()`（`crates/core/src/infrastructure/paths.rs`）は `$USAGI_HOME`（無ければ `~/.usagi`）に runtime mode 別の子 directory を足す。production は base、development は `base/dev`、local は `base/local` である。したがって **同一 workspace に対して mode を変えるだけで別の lock file になり、fence が発火しない**。

```text
cwd = /repo,  USAGI_HOME=/H,  USAGI_RUNTIME_MODE=local       → lock /H/local/daemon/daemon.lock
cwd = /repo,  USAGI_HOME=/H,  USAGI_RUNTIME_MODE=production  → lock /H/daemon/daemon.lock
  ⇒ 2 つの daemon がともに lock を取得し、ともに /repo の worktree と branch を所有する
```

この状態は lifecycle state が二重になる。lifecycle state は `<data_dir>/daemon/sessions.json` に保存され（`open_session_runtime` は `data_dir.join("daemon")` を state dir に渡す）、共有される物理資源は `<repo>/.usagi/sessions/*` の git worktree である。すなわち **独立した 2 つの durable state が同一の worktree 集合を権威として書き換える**。具体的な破損経路は次のとおり。

- daemon A が session `foo` を作る（worktree と `usagi/foo` branch が存在する）。daemon B の state には `foo` が無いので、B は `session_create foo` を重複と判定せず `git worktree add` へ進み、既存 worktree / branch と衝突する。
- daemon B の `session_remove` は、A が available として追跡している worktree を削除できる。A の state には残るため、以後 A の scope 解決は存在しない path を指す。
- どちらの daemon も相手の live PTY / Agent runtime を知らないため、`generations` fence（[05-daemon.md#generation-と-orphan-safety](../../document/05-daemon.md#generation-と-orphan-safety)）は data dir 単位でしか働かず、workspace 単位の二重所有を検出しない。

現行では v2 daemon が日常運用に載っていないため（出荷バイナリは v1 コードで daemon を持たない → memory 参照）実害の観測はまだ無い。ただし `Taskfile.yml` は `task run`（local）/ `task dev`（development）/ `task prd`（production）を並べており、同一 repo で mode を切り替える運用が前提になっているため、**v2 出荷時に確実に踏む**。潜在ではあるが severity は high である。

なお付随して見つかった別の gap も記録する。IPC handshake は client の workspace root を検証しない。`data_dir` を共有する workspace B の client が、workspace A に束縛された daemon へ接続すると、A の session 一覧と scope をそのまま受け取る。fence を workspace 単位にすると「1 workspace 1 daemon」は成立するが、この「別 workspace の client が誤って接続する」経路は fence では閉じないため、handshake 側の検証として本 issue の受入条件に含める。

## 設計判断（採用と却下）

正本は [document/proposals/13-daemon-singleton-and-teardown.md](../../document/proposals/13-daemon-singleton-and-teardown.md)。

**採用: workspace 単位の lock を追加する（data dir 単位の lock は維持する）。**

- lock path は canonical 化した repo root 直下の `<repo>/.usagi/daemon.lock` とし、**mode の子 directory の下に置かない**。これにより `local` / `dev` / `production` と `$USAGI_HOME` の表記差がすべて同一 file へ収束する。
- `flock` は inode に対する排他なので、path の綴り違い・symlink・macOS の `/tmp` → `/private/tmp` firmlink では回避できない。canonical 化は「同じ repo を別 path 表記で開いた場合」に同じ path を選ぶためのものである。
- 取得順序は **workspace lock → data dir lock** に固定する（順序固定なので deadlock しない）。両方とも endpoint 公開の ready hook より前に取得する。
- workspace lock を取れない場合は、data dir lock 失敗と同じ typed refusal 経路に載せる。message には workspace path と、読み取れる場合は所有 daemon の pid を含める。
- lock node の secure create / reopen 契約は既存 4 node（`bootstrap.lock` / `daemon.lock` / `record.lock` / `current.lock`）と同じものを再利用する（`O_NOFOLLOW | O_CLOEXEC`、`0600`、regular / owner / `nlink == 1` 検証）。
- lock node は `<repo>/.usagi` 直下ではなく daemon-private な `<repo>/.usagi/daemon/`（`0700`）の中に置く。既存の private lock node 契約は親 directory が `0700` であることを要求するが、`<repo>/.usagi` 自体は利用者が見る project metadata であり狭めるべきでないためである。
- git 追跡外にする ignore rule は追加不要である。`USAGI_GITIGNORE`（`crates/core/src/infrastructure/gitignore.rs`）は `<repo>/.usagi/.gitignore` に `/*` を書き、`issues/` と `memory/` だけを再包含する。`daemon/` はすでに ignore される（`git check-ignore` で確認済み）。

**却下: 起動経路（launchd plist / MCP 注入 / shell）の env 解決を統一する。**

- launchd plist と MCP 注入は統一できるが、利用者自身の shell（`USAGI_RUNTIME_MODE=production usagi ...`、`task prd`）は統一を強制できない。env の合意は運用規約でしか守れず、invariant にならない。
- lock は表記に依らない invariant なので、fence の正しい実装は lock 側である。ただし plist が mode を明示せず暗黙の既定に依存しているなら、それは別途 hygiene として明示する（本 issue の副作用ではなく確認項目）。

**semantics の明文化**: 「1 machine × 1 canonical workspace root に daemon は 1 つ」。mode split が分離するのは **data** であり、**workspace の所有権ではない**。git worktree は共有された物理状態なので、mode を分けても分離できない。

## やること

- workspace 単位の lock 取得を `serve` の startup に追加する（順序固定・ready hook 前）。
- canonical workspace root の解決を 1 か所に集約する（fence と `SessionRuntime` の repo root が同じ値を使うことを保証する）。
- workspace lock 失敗時の typed refusal を出す。owner の pid は fence node に書いた hint から読む（別 data directory の `daemon.json` は読めないため、これが cross-mode の唯一の発見経路になる）。
- `tests/cli_tui.rs` の production / local 併存 test は、[#540](540-fix-daemon-daemon-serve-self-shutdown-test-fixture-workspace.md) が入れた fixture workspace helper（`tests/support/daemon.rs`）を前提にする。**この依存があるため #540 の後に着手する**。

### 本 issue から分離したもの

**IPC handshake の workspace root 検証は [#548](548-fix-ipc-handshake-client-workspace-root.md) へ分離した。** fence は「1 workspace に daemon は 1 つ」を保証するが、「client が意図しない workspace の daemon へ接続する」経路は fence では閉じない。これは wire schema の追加（`ClientHello` の新 field と後方互換）と、「client の cwd をどう workspace 識別子に変換するか」「workspace 外から実行する正当な経路をどう免除するか」という別の設計判断を伴う。fence と同じ変更に混ぜると、その判断を急いで下すことになる。

`daemon status` / `daemon start` の表示は変更不要だった。fence を取得するのは `serve` だけであり、`start` は detached `serve` を spawn してその出力ではなく record の登録を待つ。workspace を他の daemon が所有していれば `serve` が record を書かずに終了するため、`start` は既存の「起動を確認できない」経路をそのまま通る。

## 受入条件

- 同一 workspace に対し `USAGI_RUNTIME_MODE` と `$USAGI_HOME` をどう変えても、2 つ目以降の `daemon serve` は起動せず typed refusal を返す。integration test で mode 差・HOME 差・path 綴り差（末尾スラッシュ、symlink 経由）の各組合せを固定する。
- 別 workspace に対する daemon は従来どおり並行起動できる（test の並列実行が壊れない）。
- 既存の start / stop / status / restart / replace、generation rollover の挙動が変わらない。カバレッジ 100% を維持する。
- [document/05-daemon.md](../../document/05-daemon.md) に fence の単位（workspace × data dir の 2 段）を畳み込む。
