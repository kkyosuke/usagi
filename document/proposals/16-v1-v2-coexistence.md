# 16. v1 / v2 の同一インストール先での共存

> [設計提案一覧](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [session garden](15-session-garden.md)

同じマシン・同じ workspace で v1 と v2 の両方を使えるようにするための設計である。

結論から述べる。**共存はほぼ既に成立している**。global / project の runtime state は
runtime mode（既定 `local`）で分離済みで、issue / memory / issue 番号は**共有が目的**であり
v2 が v1 互換の allocator を実装している。session worktree の名前空間は共有だが、衝突は
`git worktree add` の失敗として fail-closed する。残る真の衝突は **3 点**で、うち共存を実際に
阻んでいるのは配布 binary の path 1 点だけである。

## 目次

- [前提と制約](#前提と制約)
- [領域ごとの共有・分離の実態](#領域ごとの共有分離の実態)
- [衝突する 3 点](#衝突する-3-点)
- [設計 1: binary を channel 名で並置する](#設計-1-binary-を-channel-名で並置する)
- [設計 2: `.usagi/.gitignore` の行順を v1 に揃える](#設計-2-usagigitignore-の行順を-v1-に揃える)
- [設計 3: production mode の重なりを doctor で可視化する](#設計-3-production-mode-の重なりを-doctor-で可視化する)
- [衝突しないと確認した点](#衝突しないと確認した点)
- [却下した代替案](#却下した代替案)
- [実装 issue](#実装-issue)

## 前提と制約

- **共存は移行期の要求**である。v2 が release channel を取れば v1 は退役し、v2 production が
  `~/.usagi` を引き継ぐ。したがって恒久的な構造（version 別の path 階層など）を増やさない。
- **v1 は出荷物なので変更しない**。v1 は `v1-test.yml` / `v1-coverage.yml` /
  `release-build-check.yml` の gate 対象で、変更 risk とコストが v2 側の変更より高い。
  共存のための変更は v2 と配布 script に閉じる。
- **issue / memory / issue 番号は分離しない**。これを共有できることが「v1 と v2 を同時に使う」
  ことの目的そのものである。

## 領域ごとの共有・分離の実態

`<base>` は mode を適用する前の base directory（`$USAGI_HOME` または `~/.usagi`）を指す。
v2 は選択した runtime mode の子 directory を使い、既定は `local` である
（正本は [5. daemon#daemon data directory](../05-daemon.md#daemon-data-directory)）。

| 領域 | v1 が見る path | v2 が見る path（既定 = local） | 実態 |
|---|---|---|---|
| 配布 binary | `<base>/bin/usagi` | `<base>/bin/usagi` | **衝突**（mode で分岐しない） |
| global 設定・workspace 一覧 | `<base>/workspaces.json` / `settings.json` / `.lock` | `<base>/local/` 配下の同名 | 分離済（production だけ重なる） |
| global agent / pane cache | `<base>/agent-prompts/` / `agent-state/` / `open-panes/` / `unite-set.json` など | v2 は同名を使わない | 衝突なし |
| daemon 内部状態 | 持たない | `<base>/local/daemon/` | 衝突なし |
| error log | `<base>/logs/` | `<base>/local/logs/` | 分離済（production だけ重なる） |
| project machine state | `<repo>/.usagi/state.json` / `settings.json` | `<repo>/.usagi/local/` 配下 | 分離済（production だけ重なる） |
| issue Markdown | `<repo>/.usagi/issues/` | 同一 | **意図的に共有** |
| memory Markdown | `<repo>/.usagi/memory/` | 同一 | **意図的に共有** |
| issue 番号 authority | `<git-common-dir>/usagi/issue-numbers/` | 同一 | **意図的に共有**（下記） |
| `.usagi/.gitignore` | v1 の行順で書く | v2 の行順で書く | **衝突**（tracked file の churn） |
| session worktree | `<repo>/.usagi/sessions/<name>` | 同一 | 名前空間を共有（fail-closed） |
| session branch | `usagi/<name>` | 同一 | 同上 |
| workspace fence | 取らない | `<repo>/.usagi/daemon/daemon.lock` | v1↔v2 の相互排他は無い |
| trash / removals / clean.log | `<repo>/.usagi/trash/` など | 使わない | 衝突なし |

issue 番号 authority の共有は偶然ではない。v2 の `IssueNumberSequence` は sequence file を
v1 と同じ envelope（`version` + `last_reserved`）で読み書きし、`migration_floor` を v1 が無視する
追加 field として置く。v1 の存在下で採番を進めても番号が重複しないことは、v2 側の
`completed_migration_remains_bidirectionally_compatible_with_v1` などの test が固定している。

## 衝突する 3 点

```
(a) 配布 binary       <base>/bin/usagi         ← 実体が 1 つしか置けない  ★共存を阻む
(b) .gitignore 行順   <repo>/.usagi/.gitignore ← 交互起動で毎回書き換わる
(c) production mode   <base>/ と <repo>/.usagi ← 明示選択したときだけ重なる
```

`(a)` が唯一の実質的な blocker である。install script と `usagi update` はどちらも
`<base>/bin/usagi` へ rename するため、後から入れた側が前の側を消す。さらに現在の公開 release は
v1 なので、**ソースから起動した v2 で `usagi update` を実行すると v2 自身が v1 に置き換わる**。

`(b)` は v1 と v2 の `USAGI_GITIGNORE` が `.lock` と `.derived-dirty` の行順だけ異なるために起きる。
どちらの writer も「内容が完全一致しなければ書く」idempotent 実装なので、v1 で開き v2 で開くと
tracked file が毎回 dirty になる。機能影響は無いが、git status を汚し、session の PR に無関係な
差分を混ぜる。

`(c)` は `USAGI_RUNTIME_MODE=production` を明示したときだけ起きる。v2 の既定は `local` で、
production は明示指定を要求するため既定運用では踏まない。ただし踏んだときは v1 の
`workspaces.json` / `settings.json` / `state.json` を v2 の schema で上書きしうる。

## 設計 1: binary を channel 名で並置する

`scripts/install.sh` に install 先の basename を選ぶ入口を足し、v2 を別名で並置する。

```
<base>/bin/usagi     v1（公開 release。既定の名前を維持する）
<base>/bin/usagi2    v2（channel 名を指定して導入する）
```

| 項目 | 内容 |
|---|---|
| 入口 | 環境変数 `USAGI_BIN_NAME`（既定 `usagi`）。`TARGET="$BIN_DIR/$USAGI_BIN_NAME"` |
| 検証 | 名前は `[A-Za-z0-9_-]+` に限る。path 区切りを含む値は拒否して `BIN_DIR` の外へ書かせない |
| update lock | `<base>/update.lock` は**共有のまま**にする。同じ directory への rename を直列化するのが目的なので、channel ごとに分けると本来直列化したい組み合わせが漏れる |
| version 検証 | staged binary の `version` 出力と release version の一致検証は現行のまま使う |

`usagi update` 側は、**自分の実行ファイルの basename を installer へ渡す**。これで v2 の update が
v1 を上書きすることも、v2 自身が v1 に化けることもなくなる。v2 の release channel が無い間は
`usagi2` に対する update が v1 の archive を掴むため、**channel に対応する release が無いことを
検出して拒否する**のが安全側である（誤って v1 binary を `usagi2` として置かない）。

`update.rs` は `install.sh` を `include_bytes!` で同梱し、digest を unit test で固定しているので、
script 変更と digest 更新は同じ変更に含める。

## 設計 2: `.usagi/.gitignore` の行順を v1 に揃える

v2 の `USAGI_GITIGNORE` の 2 行を入れ替え、v1 と byte 一致させる。意味は変わらず、churn だけが消える。
v1 が出荷物である以上、揃える側は v2 である。

```
/issues/index.json
/issues/.lock          ← v1 の順序に合わせる（現在の v2 は .derived-dirty が先）
/issues/.derived-dirty
```

両者が byte 一致することを回帰として固定したいが、v2 の test から v1 の定数は参照できない
（`v1/` は workspace から exclude されている）。期待値を v2 側の test に literal として置き、
「v1 と一致させる意図」を doc comment に書く形をとる。

## 設計 3: production mode の重なりを doctor で可視化する

production mode の重なりは**起動を拒否しない**。v2 が v1 を置き換えるときの正規経路が
まさに production だからで、そこを fail-closed にすると cutover 自体を塞ぐ。代わりに
`usagi doctor` の診断項目として可視化する。

| 条件 | 表示 |
|---|---|
| mode が production かつ `<base>/workspaces.json` が v1 由来（v1 だけが書く同階層の cache が同居） | v1 と同じ data directory を共有していることを警告し、`USAGI_RUNTIME_MODE` の指定方法を案内する |
| mode が production 以外 | 何も出さない |

## 衝突しないと確認した点

共存の設計として重要なのは「衝突しない」ことを確認できた点である。以下は変更不要である。

- **v2 daemon は `<repo>/.usagi/sessions/` を列挙しない**。session の path は常に自分の
  lifecycle state（`sessions.json`）の session 名から組み立てる。したがって v1 が作った
  session worktree を v2 が orphan と誤認して削除することはない。逆方向も、v1 は自分の
  state に無い worktree を掃除しない。
- **session 名の衝突は fail-closed** する。同名の worktree が既にあれば `git worktree add` が、
  同名の branch が既にあれば branch 作成が失敗する。取り違えではなくエラーになる。
- `<repo>/.usagi/sessions/` を読むのは v2 の issue 番号 legacy 走査だけで、**read-only** に
  legacy issue store の floor を見るためである。v1 の worktree を壊さない。
- v1 の `trash/` / `removals/` / `clean.log` / `orchestrator-workers/` と v2 の `daemon/` は
  名前が重ならない。

一方で、**同じ workspace を v1 と v2 で同時に開くことは機械的に防げない**。v2 は
workspace fence（[5. daemon#単一 daemon の 2 段 fence](../05-daemon.md#単一-daemon-の-2-段-fence)）で
自分同士を排他するが、v1 はこの fence を取らない。v1 に fence を後付けしない方針なので、
これは**運用上の制約として残す**：同じ workspace を同時に開かない。別 workspace であれば
（issue / memory / 採番を共有しつつ）同時に使ってよい。

## 却下した代替案

| 代替案 | 却下理由 |
|---|---|
| `SESSIONS_DIR` を runtime mode で分岐させ、`<repo>/.usagi/local/sessions/` にする | session create / remove / resume / lifecycle state / issue store の session 判定に広く波及する。移行期の要求に対して恒久構造を増やすコストが釣り合わない。名前空間の衝突は既に fail-closed でもある |
| branch prefix を `usagi2/` に分ける | 同上に加え、session 名から branch 名を導く規約（`usagi/<name>`）が v1/v2 で分岐し、PR 検出・merge 判定の path が二重化する |
| v1 に workspace fence を後付けする | v1 は出荷物であり、fence の追加は daemon を持たない v1 の lifecycle に新しい失敗モードを持ち込む。移行期のためにリリース中の実装へ risk を入れない |
| production mode の base に major version 層（`~/.usagi/v2`）を挟む | v2 が v1 を置き換えたあとも残る恒久的な path 階層になる。cutover 後に不要な層を剥がす移行が別途必要になる |
| v2 で `usagi update` を無効化する | 共存とは無関係に v2 の update 経路を失う。channel 名を渡す（設計 1）ほうが小さい |

## 実装 issue

| # | 設計 | 内容 |
|---|---|---|
| [#690](../../.usagi/issues/690-feat-cli-install-sh-binary-channel-v1-v2.md) | 設計 1 | `install.sh` の `USAGI_BIN_NAME` と、`usagi update` の channel 名引き渡し・release 不在時の拒否 |
| [#691](../../.usagi/issues/691-fix-core-usagi-gitignore-v1-churn.md) | 設計 2 | v2 `USAGI_GITIGNORE` の行順を v1 に揃える |
| [#692](../../.usagi/issues/692-feat-cli-doctor-production-mode-v1-data-directory.md) | 設計 3 | `usagi doctor` の production data directory 共有の警告 |

設計 1 だけが共存の blocker で、設計 2・3 は共存中の摩擦を減らす。設計 2 は #690 と独立に進められる。
