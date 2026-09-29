---
number: 775
title: "feat(sandbox): Agent が MCP で追加の書き込み許可を user に求め、承認を保存して以降の起動に適用する"
status: todo
priority: medium
labels: [v2, sandbox, mcp, agent, daemon, tui, security, feature]
dependson: []
related: [702, 608]
created_at: 2026-09-30T00:00:00+00:00
updated_at: 2026-09-30T00:00:00+00:00
---

## 背景

session agent は `usagi claude-sandbox --mode session` の中で動き、書き込めるのは session worktree・Git の最小限の
admin state・`$TMPDIR` / `/tmp` / `/var/tmp`・agent 自身の state・macOS の Keychain / MDS だけである
（正本は [2. アーキテクチャ](../../document/02-architecture.md#claude-起動の多層防御)、
`usagi_core::usecase::claude_sandbox::writable_roots`、`claude_writable_roots`）。

このため、パッケージマネージャが `$HOME` 配下のキャッシュへ書く処理は、session の中では全部失敗する。

| 事象 | 原因 |
|---|---|
| `bun install` が `bun is unable to write files to tempdir: PermissionDenied` で失敗する。依存が欠けた e2e テストは ENOENT の後も終了せず、lefthook の pre-push が永久に待つ（別 project で発生） | `~/.bun/install/cache`（と、キャッシュと同じ volume に作る tempdir）が writable でない |
| usagi 自身の session で `cargo check` / `test` が一切動かない | `~/.cargo/registry` に locked crate を download できない |

今の回避策は、user が sandbox の外で `bun install` / `cargo fetch` を手で打つか、`git push --no-verify` を使うことで、
どちらも毎回 user の手を止める。`!` prefix で打っても Claude Code の子プロセスとして同じ sandbox に入るので、回避にならない。

## 目的

Agent が sandbox に阻まれたとき、**MCP で user に書き込み許可を求める。user が承認したら grant を保存し、以降の Agent
起動では確認なしに書き込めるようにする**。ただし、sandbox が hard boundary であるという性質は崩さない。つまり、
許可された書き込みを使って sandbox の外のコードを乗っ取れないようにする。

## 設計

### 全体の流れ

```text
Agent (sandbox 内)                 daemon                               user (TUI)
  │ sandbox_grant_request          │                                    │
  │   {path, reason}  ───────────▶ │ 1. path を正規化・検証（危険 path は即拒否）
  │                                │ 2. daemon が文面を組んだ user decision を作成
  │ ◀── Pending (decision_id) ──── │ ──────── pending decision overlay ─▶ │
  │ user_decision_get で polling   │                                    │ 「常に許可」/「拒否」
  │                                │ ◀─────────── resolve ─────────────── │
  │                                │ 3. 承認なら grant store へ durable 保存
  │ ◀── Approved (要再起動) ─────── │                                    │
  │ （再起動 / resume 後の launch から writable root に含まれる）
```

### 1. MCP tool `sandbox_grant_request`

| 入力 | 型 | 内容 |
|---|---|---|
| `path` | `string` | 書き込みたい directory の absolute path |
| `reason` | `string` | user に見せる理由（上限 2 KiB。非信頼テキストとして表示する） |
| `idempotency_key` | `string?` | 既存の user decision と同じ意味論 |

- 既存の `user_decision_*` の durable store・TUI overlay・polling 契約（[7. MCP](../../document/07-mcp.md)）をそのまま使い、
  新しい待機 protocol は作らない。
- **決定の文面と選択肢は daemon が組み、agent は path と reason だけを渡す**。agent に任意の選択肢 ID を作らせると、
  「承認」に見える別の選択肢や、正規化前と違う path を user に見せられてしまうためである。表示するのは、正規化後の
  canonical path・適用範囲（この workspace の全 session）・リスクの 1 行説明・agent の reason である。
- 選択肢は `allow_always`（保存する）と `deny` の 2 つにする。「今回だけ許可」は第 1 段階では入れない。実行中の
  sandbox profile は後から広げられない（次項）ため、効果が「次の launch だけ」になって分かりにくいからである。
- 呼べるのは session mode の agent だけにする。root coordinator は repository を read-only に保つ設計なので、
  root 用の grant は作らない。

### 2. 適用のタイミング

`sandbox-exec` の profile も `bwrap` の bind も exec 時に固定されるため、実行中の agent に writable root を後から
足すことはできない。承認した grant が効くのは**次の Agent launch から**である。

- tool の結果（decision の terminal 状態）には、承認されたことと、反映には agent の再起動が必要なことを明記する。
- 第 1 段階では、再起動は user が既存の resume 経路で行う。承認をきっかけに daemon が自動で relaunch する機能は、後続の issue に分ける。

### 3. grant store（保存先）

| 項目 | 決定 |
|---|---|
| 置き場所 | data home 配下の daemon 所有ファイル（例: `<data dir>/sandbox-grants.json`）。**`.usagi/settings.json` や `.usagi/config.toml` には置かない**。これらは repository に入るので、checkout 側が grant を持ち込めてしまう |
| 書き手 | daemon だけ。data home は sandbox の writable root に入らない（既存の不変条件）ので、agent が自分で grant を書き足すことはできない |
| key | `(workspace id, canonical path)`。全 workspace への一括 grant は作らない |
| 値 | canonical path・device/inode などの identity・承認時刻・decision ID（監査用） |
| 反映 | Agent launch ごとに `claude_writable_roots` の結果へ足し、`launch_roots` として `SandboxRequest` に渡す。その際、exec 直前の既存の検証（absolute canonical directory・owner・symlink identity・protected ancestor）を必ずもう一度通す |
| 取り消し | TUI の Workspace Config に `Sandbox grants` 行を置き、一覧と削除を提供する。agent から取り消す tool は第 1 段階では作らない |

### 4. 危険 path の拒否（最も重要）

「user が承認したら何でも writable」にすると、1 回の承認で sandbox が意味を失う。sandbox の外で後から実行されるコードや
設定を書き換えられる path は、daemon が user に確認する**前に**拒否する（承認されても保存しない）。

| 拒否するもの | 理由 |
|---|---|
| `/`、`$HOME` そのもの、`$HOME` の祖先 | 全面開放と同じになる |
| 保護対象 workspace・他 session の worktree・Git common dir・data home と、その祖先 / 子孫 | 既存の repository 書き込み境界と daemon state を壊す |
| `PATH` の各 entry（例: `~/.cargo/bin`, `~/.bun/bin`, `/usr/local/bin`）を含む、またはその中にある path | バイナリを差し替えると、user が sandbox の外で実行したときに任意のコードが実行される |
| shell / tool の起動設定: `~/.zshrc` などの rc、`~/.config`、`~/.gitconfig`、`~/.ssh`、`~/Library/LaunchAgents`、`~/.cargo/config*`、`~/.bunfig.toml` などを含む path | 起動時の実行・credential・Git hook を乗っ取られる |
| 既存の read-only carve-out（provider の global customization、AGY plugin root など）と重なる path | 既存の多層防御を迂回できてしまう |

- 判定は `usagi-core` の純粋関数（例: `usecase::claude_sandbox::grant_admission`）にまとめ、全分岐を unit test で固定する。
- 拒否したときは、tool の結果でより狭い代替を案内する。例: `~/.cargo` なら `~/.cargo/registry` と `~/.cargo/git`、
  `~/.bun` なら `~/.bun/install/cache`。

### 残るリスク（承認の文面で user に伝える）

キャッシュ directory を writable にすると、session の agent がそのキャッシュを汚染できる。汚染された依存を user が
sandbox の外でビルド・実行すると（`build.rs`、`postinstall` など）、sandbox の外でコードが実行される。cargo は
`registry/src` の展開済みソースを再検証しないし、bun もキャッシュ済みのパッケージを信頼して再利用する。
このリスクは path の拒否では消せないので、次の対策を取る。

- 承認の文面に上記の 1 行説明を必ず含める。
- grant は workspace 単位で key し、一括 grant は作らない。
- 代替として per-session キャッシュを案内する。例えば workspace env に `BUN_INSTALL_CACHE_DIR=$TMPDIR/...` を設定すれば、
  共有キャッシュを汚さずに済む。

## 受け入れ条件

- [ ] session agent が `sandbox_grant_request` を呼ぶと、TUI に pending decision が表示される。承認後の次の launch では、
      その path に書き込める（macOS の `sandbox-exec`、Linux の `bwrap` の両方）。
- [ ] 拒否したとき、または deadline が切れたときは、grant を保存せず、launch の writable root も変わらない。
- [ ] 危険 path の表の全行が、user に確認する前に拒否される（unit test で確認する）。PATH entry の判定には daemon bootstrap の
      trusted environment を使い、agent child の `PATH` は使わない。
- [ ] grant は data home の daemon 所有ファイルにだけ保存される。`.usagi/` 配下の設定からは grant を追加できない。
- [ ] launch 時の再検証で、symlink が差し替えられた grant・owner が変わった grant・削除された grant は writable root に入らない（fail closed）。
- [ ] root mode の agent からの request は拒否される。
- [ ] TUI の Workspace Config で grant を一覧・削除できる。
- [ ] `document/02-architecture.md`（sandbox の writable root）、`document/07-mcp.md`（tool）、`document/03-tui.md`（config 行）を更新する。
- [ ] coverage 100% を維持する。

## 範囲外（後続の issue）

- 承認をきっかけに daemon が Agent を自動で relaunch する。
- 「今回の session だけ許可」の選択肢。
- per-session キャッシュ env（`BUN_INSTALL_CACHE_DIR` / `CARGO_HOME` 相当）を daemon が自動で設定する。
