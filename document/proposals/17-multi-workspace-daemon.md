# 17. workspace ごとの daemon（複数 workspace の同時利用）

> [設計提案一覧](README.md) ｜ 関連仕様: [daemon](../05-daemon.md) ｜ [daemon IPC](../04-ipc.md) ｜ [TUI](../03-tui.md) ｜ 関連提案: [daemon の単一インスタンスと teardown](13-daemon-singleton-and-teardown.md)

daemon の durable state を **workspace 単位**に分割し、`workspace A の daemon` と `workspace B の daemon` が
同じ machine・同じ data directory で同時に動けるようにする設計である。

現在は data directory ごとに active daemon が 1 つで、その daemon が serve する workspace も 1 つに固定される。
そのため Welcome の Open / Recent に並ぶ workspace のうち、**いま daemon が serve している 1 つ以外は開けない**。

```text
$ usagi            # daemon は AccelHack を serve している
cannot open /Users/…/usagi: this daemon does not serve the selected workspace;
this daemon serves the workspace /Users/…/AccelHack.
Stop it with `usagi daemon stop`, then start usagi in /Users/…/usagi.
```

この拒否は設計どおりの typed refusal（[workspace fence](../04-ipc.md#workspace-fence)）であり、誤動作ではない。
本書が変えるのは fence ではなく、**fence の手前で client が「その workspace の daemon」に到達できるようにする
state の置き場所**である。

## 目次

- [目標と非目標](#目標と非目標)
- [制約がどこから来るか](#制約がどこから来るか)
- [機構](#機構)
  - [workspace-scoped な daemon state directory](#workspace-scoped-な-daemon-state-directory)
  - [digest と衝突](#digest-と衝突)
  - [socket path の長さ予算](#socket-path-の長さ予算)
  - [client から workspace への解決](#client-から-workspace-への解決)
  - [generation socket の sweep 分離](#generation-socket-の-sweep-分離)
  - [legacy layout からの migration](#legacy-layout-からの-migration)
  - [CLI の workspace 選択](#cli-の-workspace-選択)
  - [TUI の切り替え](#tui-の切り替え)
- [変えないもの](#変えないもの)
- [増える daemon process の扱い](#増える-daemon-process-の扱い)
- [却下した代替案](#却下した代替案)
- [段階と issue 分割](#段階と-issue-分割)
- [test 戦略](#test-戦略)

## 目標と非目標

| | 内容 |
|---|---|
| 目標 | 別 workspace を開くのに **先行 workspace の daemon を止めなくてよい**（live Agent を殺さずに切り替えられる） |
| 目標 | workspace A の TUI と workspace B の TUI を**同時に**開ける（別端末・別 process） |
| 目標 | Recent / global settings / logs は今までどおり **1 つの data directory に共有**する |
| 目標 | 切り替えの拒否が消えても、**誤った daemon に到達したときの fence は残る**（refusal は backstop として保持） |
| 非目標 | 1 つの daemon process が複数 workspace を serve すること（[却下した代替案](#却下した代替案)） |
| 非目標 | 1 つの TUI process が複数 workspace へ同時接続すること（[workspace の離脱と終了](../03-tui.md#workspace-の離脱と終了)の契約は不変） |
| 非目標 | runtime mode（`production` / `development` / `local`）の分離規則の変更 |
| 非目標 | 同一 workspace を 2 つの daemon が所有できるようにすること（workspace fence は不変） |

## 制約がどこから来るか

「別 workspace を同時に扱えない」は 1 か所の判定ではなく、**state の置き場所**から生じている。

| # | 出どころ | 現在 | 効果 |
|---|---|---|---|
| C1 | 単一インスタンス lock `<data-dir>/daemon/daemon.lock` | data directory ごとに 1 つ | 2 つ目の daemon は data directory が同じ限り起動できない |
| C2 | locator `<data-dir>/daemon/current.json` | data directory ごとに 1 つ | client は「その data directory の daemon」しか発見できない |
| C3 | lifecycle state `<data-dir>/daemon/sessions.json` | `repository_root` を 1 つ持つ | 2 つ目の workspace の session lifecycle を書く場所が無い |
| C4 | generation registry / allocator / shard / dispatch / inbox / PR inventory | data directory ごとに 1 つ | runtime 権威が workspace をまたいで 1 本になっている |

workspace fence（`<workspace>/.usagi/daemon/daemon.lock`）は **workspace ごと**なので C1〜C4 とは独立であり、
本提案でも変えない。すなわち解くべきは C1〜C4 の scope であって、fence ではない。

現行の docs（[04-ipc.md#workspace-fence](../04-ipc.md#workspace-fence)、[03-tui.md#workspace-の選択と-daemon](../03-tui.md#workspace-の選択と-daemon)）
はこの制約を「daemon は 1 つ、serve する workspace も 1 つ」と明記しており、本提案が実装されたら
その記述を畳み込み直す。

## 機構

### workspace-scoped な daemon state directory

C1〜C4 の node を **canonical workspace root の digest で分けた subtree**へ移す。data directory 自体は分けない
（Recent・global settings・logs・agent state は machine 単位のままにする）。

```text
<data-dir>/
  settings.json          global 設定             ← 共有のまま
  workspaces.json        workspace registry      ← 共有のまま（Recent が割れない）
  logs/                  日次 error log          ← 共有のまま（entry に workspace を持たせる）
  daemon/
    bootstrap-broker-<digest>.{lock,sock}        ← 既に workspace × executable 単位。そのまま
    w/
      <workspace-digest>/
        root.json        この subtree が属する canonical workspace root（診断と衝突検出）
        daemon.json      lifecycle record          ┐
        daemon.lock      単一インスタンス lock      │
        bootstrap.lock   client bootstrap の直列化  │
        record.lock / current.lock / current.json  │ すべて
        generations.json / generations.lock        │ workspace 単位へ
        allocations.json / allocations.lock        │
        shards/<generation>.{json,lock}            │
        g/<generation>/sock                        │
        sessions.json / runtime-migration.json     │
        pr-inventory.json / dispatch.json          │
        inbox/<caller-session-id>/<caller-agent-id>.jsonl ┘
```

置き場所の決定は **1 つの resolver**（`daemon_state_dir(data_dir, workspace_root)`）に集約し、daemon 側と client 側の
どちらもそこを通す。今 `data_dir.join("daemon")` を直接書いている経路が 1 つでも残ると、片側だけ legacy layout を
見て「daemon が居ないので cold start」を無限に繰り返すため、この集約が実装上の中心になる。

単一インスタンス lock（C1）は workspace 単位になっても残す。workspace fence と役割が違うためである。

| fence | 何を拒否するか |
|---|---|
| workspace fence `<workspace>/.usagi/daemon/daemon.lock` | 同じ workspace を所有する 2 つ目の daemon（**mode と data directory を問わない**） |
| 単一インスタンス lock `<data-dir>/daemon/w/<digest>/daemon.lock` | 同じ data directory × 同じ workspace の active role を取る 2 つ目の daemon（record・locator・shard の単一書き手） |

### digest と衝突

digest は既存の [bootstrap broker key](../05-daemon.md#sandbox-bootstrap-broker) と同じ作り方にする。
すなわち domain separation tag と length prefix を付けた SHA-256 で、先頭 6 byte を hex 12 文字にする。

```text
digest = SHA-256("usagi-daemon-workspace-v1" ‖ len(root) ‖ canonical_workspace_root)[..6]  → 12 hex
```

digest は**短縮しているので衝突しうる**。衝突を「稀だから無視する」のではなく、`root.json` で必ず検証する。

| 状況 | 動作 |
|---|---|
| `root.json` が無い | 新規 subtree として作成し、canonical root を書く |
| `root.json` の root が一致する | その subtree を使う |
| `root.json` の root が別 workspace | `-1`、`-2` … と suffix を伸ばして次の候補を見る（linear probing）。**別 workspace の state を書かない** |

`root.json` は診断にも使う。`daemon status --all` と client の [workspace 解決](#client-から-workspace-への解決)が
この 1 ファイルだけで subtree ↔ workspace の対応を復元できる。

### socket path の長さ予算

Unix domain socket の path は macOS で 104 byte（`sun_path`）、Linux で 108 byte が上限であり、
**subtree を 1 段深くすることは無条件では許されない**。現在の実測は次のとおりである。

| layout | 例 | 長さ |
|---|---|---|
| 現在 | `/Users/kyosuke/.usagi/daemon/generations/<uuid>/sock` | 82 |
| 本提案 | `/Users/kyosuke/.usagi/daemon/w/<12hex>/g/<uuid>/sock` | 87 |

`generations/` を `g/` へ縮めることで、subtree の 15 文字に対し実質 +5 文字に収める。それでも
`$USAGI_HOME` が深い利用者では上限に届きうるため、**bind の前に長さを検査**し、超える場合は
`$USAGI_HOME` を短くする復帰手順を含む typed error で拒否する。今は上限超過が bind の OS error として
表面化するだけで、原因も復帰手順も利用者に見えない。

### client から workspace への解決

locator が workspace 単位になるため、client は接続前に「どの workspace の daemon か」を決める必要がある。
申告（`selected` / `bound`）の決定順（[04-ipc.md#workspace-fence](../04-ipc.md#workspace-fence)）は変えず、
**その申告から subtree を引く規則**を足す。

| 優先 | 申告 | subtree の引き方 |
|---|---|---|
| 1 | `selected`（TUI が開いた workspace） | その canonical root の digest。無ければ cold start でその root の daemon を起こす |
| 2 | `bound`（`USAGI_WORKSPACE_ROOT`。daemon が provision した child） | 同上。injected root は trusted root そのものなので一致する |
| 3 | `bound`（cwd） | `w/*/root.json` を読み、**cwd の祖先で最長一致**する root の subtree（session worktree `<root>/.usagi/sessions/<name>` からの CLI / MCP がここで正しい daemon に届く）。一致が無ければ cwd を root と見なして cold start |
| — | `unbound`（readiness、`daemon replace`） | workspace state を読まない。到達先を必要とする操作は 1〜3 と同じ解決を使う |

3 の「祖先で最長一致」は git を実行せずに済み、非 git workspace でも成立する。session worktree 自身が
`.usagi/` を持つため、「最も近い `.usagi` を持つ祖先」では誤って session worktree を root と判定する。
**判定材料は `root.json` に記録した canonical root だけ**にする。

### generation socket の sweep 分離

現在の起動時 sweep は `generations/` を走査し、自分の registry が preserve しない generation directory の
socket を回収する。generation directory が data directory 単位のままだと、**workspace A の daemon が
workspace B の live socket を residue と判定して消す**。subtree を `w/<digest>/g/` にすることで、
sweep の走査範囲がその workspace の registry と 1 対 1 になり、preserve 判定は現在のままで正しくなる。

これは「別 workspace の tree へは書かない」という本提案の invariant そのものであり、test でも
**A の sweep 後に B の socket が生存している**ことを直接確認する。

### legacy layout からの migration

新 layout を持つ build が最初に `serve` するとき、`<data-dir>/daemon/sessions.json` が legacy 位置にあれば
その `repository_root` の subtree へ **rename で一方向移行**する。移行の記録は既存の
`runtime-migration.json` と同じ形（schema・件数・証明不能だった件数）にする。

| 手順 | 内容 |
|---|---|
| 1 | legacy 単一インスタンス lock を取得する（移行中に旧 build が active になれない） |
| 2 | legacy `sessions.json` の `repository_root` を canonical 化し、subtree と `root.json` を作る |
| 3 | durable な node（`sessions.json` / `generations.json` / `allocations.json` / `shards/` / `dispatch.json` / `inbox/` / `pr-inventory.json`）を rename で移す |
| 4 | locator・record・socket は移さず**破棄**する（旧 process は既に別 build であり、endpoint は再公開される） |
| 5 | 移行を `runtime-migration.json` に記録し、legacy 位置には何も残さない |

client は migration を行わない。legacy locator しか無い状態の client は「daemon 不在」として cold start に
落ち、起動した daemon が 1〜5 を行う。**古い build へ戻すと state が見えなくなる**（`.migrated` 退役と同じ
一方向性）ことは受け入れる。

### CLI の workspace 選択

`usagi daemon` の verb は「どの workspace の daemon か」を取るようになる。既定は
[client から workspace への解決](#client-から-workspace-への解決)の規則 3（cwd）である。

| コマンド | 変更 |
|---|---|
| `usagi daemon status` | 既定は cwd の workspace。`--workspace <path>` で指定、`--all` で data directory 内の全 workspace daemon を `root.json` から列挙する |
| `usagi daemon stop` / `restart` / `replace` | 同じ選択規則。`--all` は stop にだけ用意し、live runtime を持つ daemon の扱いは現在の契約（`--force` 必須）を保つ |
| `usagi daemon install-service` | 変更なし。supervisor は既に workspace を pin して install する |

### TUI の切り替え

Welcome の Open / Recent から別 workspace を選ぶ経路が、**拒否ではなく通常の open** になる。

| 状況 | 現在 | 本提案 |
|---|---|---|
| 選んだ workspace の daemon が動いている | 別 workspace の daemon に当たり typed refusal | その daemon へ接続して開く |
| 選んだ workspace の daemon が動いていない | 他の daemon が動いていれば refusal | その workspace で cold start して開く |
| 到達した daemon が別 workspace を serve していた | refusal（画面に留まり notice） | **refusal のまま**（[変えないもの](#変えないもの)） |

[workspace の離脱と終了](../03-tui.md#workspace-の離脱と終了)の契約は変わらない。1 process が同時に持つ daemon 接続は
1 本のままで、離脱時に前の workspace の port・pump・worker をすべて落としてから次へ接続する。同時に 2 つの
workspace を見たい利用者は、これまでどおり端末を 2 枚使う（その 2 枚が別々の daemon に届くようになる、というのが
本提案の効果である）。

## 変えないもの

| 対象 | 理由 |
|---|---|
| workspace fence（`selected` は完全一致、`bound` は配下のみ admit） | locator が workspace 単位になっても、client が誤った endpoint に届く経路（stale locator、手で書いた path、digest 衝突）は残る。fence はその backstop であり、**refusal path は削除せず test も残す** |
| 1 process 1 daemon 接続 | 複数接続は pane・pump の所有者を曖昧にする。切り替えは teardown → 再接続のままにする |
| runtime mode による data 分離 | mode は data を分ける規則であり、workspace の所有権は分けない（[13. 単一インスタンスと teardown](13-daemon-singleton-and-teardown.md)） |
| 同一 workspace の複数 TUI | 既に成立している（PTY geometry は attach 中 client の最小値） |

## 増える daemon process の扱い

workspace を開くたびに daemon が 1 つ増えるため、「開いたことを忘れた workspace の daemon が常駐し続ける」形に
なりうる。次の 2 つで抑える。

- **idle self-shutdown**（段階 5）: live runtime が無く、接続中 client も無く、durable な未完了 operation も無い状態が
  一定時間続いた daemon は、custody 喪失と同じ graceful path で自主終了して fence を返す。supervisor が install されている
  workspace は対象外にする（supervisor が即座に起こし直すため）。
- **`usagi daemon status --all` / `stop --all`**: 常駐している daemon を利用者が 1 コマンドで把握・回収できる。

## 却下した代替案

| 案 | 却下理由 |
|---|---|
| 現状維持（refusal の文面改善だけ） | Welcome の Open / Recent は登録済み workspace を全件出すのに、そのうち 1 つしか開けない。切り替えのたびに `daemon stop` が要り、live Agent を持つ daemon の stop は `--force`（= 実行中の Agent を捨てる）になる。日常運用が成立しない |
| workspace ごとに `$USAGI_HOME` を分ける（現在の回避策） | 動くが、`workspaces.json`（Recent）・`settings.json`（global 設定・env）・logs・agent state まで workspace ごとに割れる。すべての shell で env を正しく設定し続ける規律も要求する。**回避策としては案内するが、製品の答えにはしない** |
| 1 daemon が複数 workspace を serve する（multi-tenant） | daemon の権威（`sessions.json` の `repository_root` と root worktree id、generation registry、allocator、shard）が単一 workspace 前提で組まれており、全層に tenant 次元が入る。1 workspace の rollover / crash が無関係な workspace の live PTY を巻き添えにする。workspace fence を process 内で多重化する必要があり、[13](13-daemon-singleton-and-teardown.md) の単一書き手の議論をやり直すことになる |
| TUI が切り替え時に自動で stop → start する | live runtime があれば結局拒否になり、無ければ他 workspace の Agent を落とす。「別 repo の Agent を走らせたまま別 repo を触る」という usagi の中心的な使い方を諦めることになる |
| data directory 自体を workspace ごとにする | C1〜C4 は解けるが、Recent・global settings・logs まで割れるため `$USAGI_HOME` 回避策と同じ欠点を持つ |

## 段階と issue 分割

| 段階 | issue | 内容 | 独立して出荷できるか |
|---|---|---|---|
| 1 | #708 | `daemon_state_dir` resolver、workspace digest と `root.json` 衝突検出、socket path 長さ予算、legacy layout からの migration。単一 workspace の挙動は不変 | 可（挙動は変わらず layout だけ移る） |
| 2 | #709 | generation socket sweep の subtree 分離と、2 workspace 同時稼働の結合テスト | 可 |
| 3 | #710 | client 側の workspace 解決（`root.json` の最長一致）と `usagi daemon status/stop/restart --workspace/--all` | 可 |
| 4 | #711 | TUI: Open / Recent からの切り替えを refusal ではなく open にする（fence refusal path は保持）。実 PTY E2E | 可（ここで利用者に見える価値が出る） |
| 5 | #712 | idle self-shutdown（任意。段階 4 の後の運用で必要性を測ってから） | 可 |
| 6 | #711 | 正本の畳み込み（[05-daemon.md](../05-daemon.md) の data directory と 2 段 fence、[04-ipc.md](../04-ipc.md) の workspace fence、[03-tui.md](../03-tui.md) の workspace 選択） | 段階 4 と同じ PR |

## test 戦略

| 層 | 検証 |
|---|---|
| unit | digest の決定性と domain separation、`root.json` 不一致時の probing、socket path 長さ予算の境界、cwd → subtree の最長一致（session worktree からの解決を含む） |
| integration（daemon） | 2 つの fixture workspace で daemon を同時に起動し、各々が自分の session だけを serve する。A の起動時 sweep 後に B の socket が生存する。legacy layout からの migration が `sessions.json` の `repository_root` の subtree に着地する |
| integration（root） | `daemon status --all` が両方を列挙し、`stop` が指定した 1 つだけを止める。fence refusal は「別 workspace の locator を手で指した」場合に依然として出る |
| E2E（実 PTY） | workspace A を開いて Agent を live にしたまま Welcome へ戻り、workspace B を開いて操作し、A の Agent が生存していることを確認する（[重い E2E の直列化](../06-conventions.md#重い-e2e-の直列化)の列に載せる） |
