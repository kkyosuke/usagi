# 17. 1 daemon が複数 workspace を serve する（multi-tenant daemon）

> [設計提案一覧](README.md) ｜ 関連仕様: [daemon](../05-daemon.md) ｜ [daemon IPC](../04-ipc.md) ｜ [TUI](../03-tui.md) ｜ 関連提案: [daemon の単一インスタンスと teardown](13-daemon-singleton-and-teardown.md) ｜ [restart 後の状態復帰](16-restart-state-restoration.md)

1 つの daemon process が **複数の workspace を同時に serve する**設計である。workspace の追加は
process の追加ではなく、その daemon が **tenant を adopt する**ことで行う。

現在は data directory ごとに active daemon が 1 つで、その daemon が serve する workspace も起動時 cwd で
1 つに確定する。そのため Welcome の Open / Recent に並ぶ workspace のうち、**いま serve している 1 つ以外は開けない**。

```text
$ usagi            # daemon は AccelHack を serve している
cannot open /Users/…/usagi: this daemon does not serve the selected workspace;
this daemon serves the workspace /Users/…/AccelHack.
Stop it with `usagi daemon stop`, then start usagi in /Users/…/usagi.
```

この拒否は設計どおりの typed refusal（[workspace fence](../04-ipc.md#workspace-fence)）である。本書が変えるのは
fence の判定基準（「唯一の trusted root と一致するか」→「adopt 済み tenant のどれかに解決できるか」）であり、
fence そのものは残す。

## 目次

- [目標と非目標](#目標と非目標)
- [今の実装はどこまで multi-workspace か](#今の実装はどこまで-multi-workspace-か)
- [機構](#機構)
  - [tenant registry と workspace fence の多重保持](#tenant-registry-と-workspace-fence-の多重保持)
  - [handshake の admission](#handshake-の-admission)
  - [state layout](#state-layout)
  - [資源の上限](#資源の上限)
  - [CLI と TUI](#cli-と-tui)
- [変えないもの](#変えないもの)
- [受け入れるコスト](#受け入れるコスト)
- [却下した代替案](#却下した代替案)
- [段階と issue 分割](#段階と-issue-分割)
- [test 戦略](#test-戦略)

## 目標と非目標

| | 内容 |
|---|---|
| 目標 | 別 workspace を開くのに **daemon を止めなくてよい**（live Agent を殺さずに切り替えられる） |
| 目標 | workspace A の TUI と workspace B の TUI を**同時に**開ける（別端末・別 process が同じ daemon に届く） |
| 目標 | machine あたり daemon は 1 つのまま。locator・bootstrap・supervisor・`daemon status` が単数のまま保たれる |
| 目標 | 「1 machine × 1 canonical workspace root に所有者は 1 つ」の invariant を維持する（fence は残す） |
| 非目標 | 1 つの TUI process が複数 workspace へ同時接続すること（[workspace の離脱と終了](../03-tui.md#workspace-の離脱と終了)の契約は不変） |
| 非目標 | workspace をまたぐ横断ビュー（全 repo の session を 1 画面に出す等）。本提案はそれを**可能にするが、実装しない** |
| 非目標 | runtime mode（`production` / `development` / `local`）の分離規則の変更 |
| 非目標 | tenant 境界での障害隔離（[受け入れるコスト](#受け入れるコスト)） |

## 今の実装はどこまで multi-workspace か

domain 層は**すでに workspace 次元を持っている**。単一 workspace 前提なのは、その外側の薄い層だけである。

| すでに workspace 次元を持つ | 形 |
|---|---|
| `TerminalRef`（`usagi-core` domain の typed ID） | `daemon_generation` / `terminal_id` / **`workspace_id`** / `session_id?` / `worktree_id` |
| `CompletionFence` | late worker の完了 fence に `workspace_id` が必ず入る |
| terminal retention | workspace ごとの usage を `BTreeMap<WorkspaceId, Usage>` で持ち、**daemon 全体の aggregate と別に**数える |
| terminal scope / launch / user decision | いずれも `workspace_id` で fence する |
| `AgentInventory` | workspace 単位で root scope と managed session を束ねる |

retention が「daemon 全体」と「workspace ごと」を別集計にしていることが要点である。1 daemon 1 workspace が
恒久的な前提なら、この 2 段の集計は同義であり存在理由が無い。

| 単一 workspace 前提 | 形 |
|---|---|
| handshake | `ServerProtocol` が `workspace_root` を 1 本だけ持ち、`workspace_admission` がそれとの一致で判定する |
| lifecycle | `SessionRuntime` が 1 インスタンス。`sessions.json` は `repository_root` と root worktree id を 1 組だけ持つ |
| fence | `FileWorkspaceFence { path }` を `serve` の起動時に 1 つだけ取得する |
| 起動 | workspace root を起動時 cwd（durable な `repository_root` があればそちら）から 1 回だけ確定する |

`SessionRuntime` は `(repository_root, workspace_id, state store)` で parameterize されたオブジェクトであり、
`FileWorkspaceFence` は path を持つだけの値である。したがって本提案の中心は**この 4 行を単数から複数へ広げること**であり、
domain と wire protocol の作り直しではない。

## 機構

### tenant registry と workspace fence の多重保持

daemon は adopt 済み workspace を tenant registry に持つ。tenant 1 件は「canonical root・`WorkspaceId`・
保持中の workspace fence guard・`SessionRuntime`」の組である。

```text
daemon process (1 machine あたり 1 つ)
├─ tenant /…/AccelHack   fence guard ─ SessionRuntime ─ sessions.json(A)
├─ tenant /…/usagi       fence guard ─ SessionRuntime ─ sessions.json(B)
└─ 共有:  locator / bootstrap / generation registry / allocator / shard / retention aggregate
```

| 契機 | 動作 |
|---|---|
| 起動 | 起動時 cwd（または durable な `repository_root`）の workspace を **initial tenant** として adopt する。現行の起動挙動と同じ |
| adopt | client が未 adopt の workspace を申告した時点で、canonical 化 → workspace fence 取得 → `SessionRuntime::open` → registry 登録、の順に行う。順序は現行 `serve` の取得順（workspace fence → state）と同じ |
| fence が取れない | **その workspace だけ** typed refusal（owner pid を添える）。他 tenant は影響を受けない。別 mode・別 build の daemon が正当に所有している場合がこれにあたる |
| retire | tenant に live runtime も未完了 durable operation も無く、参照する client も無い状態が続いたら fence と `SessionRuntime` を解放する（段階 5） |
| 停止 | shutdown はすべての tenant を graceful に閉じ、fence を逆順に返す |

adopt は 1 workspace につき直列化し、同時 adopt 数と tenant 総数に上限を置く（[資源の上限](#資源の上限)）。

### handshake の admission

[workspace fence](../04-ipc.md#workspace-fence) の申告（`unbound` / `bound` / `selected`）と決定順は変えない。
変えるのは daemon 側の判定である。

| 申告 | 現在 | 本提案 |
|---|---|---|
| `selected` | 唯一の trusted root と完全一致なら admit | adopt 済み tenant と完全一致なら admit。未 adopt なら **その場で adopt** して admit |
| `bound` | 唯一の trusted root の配下なら admit | adopt 済み tenant のいずれかの配下なら admit（最長一致でその tenant へ解決）。どれにも属さなければ、その root を adopt して admit |
| `unbound` | admit | 変更なし |

refusal path は残る。fence を他 process が持つ、root が非 UTF-8 で綴れない、tenant 上限に達した、の 3 つが
拒否理由になり、いずれも **workspace 単位**の拒否であって接続全体の否定ではない。message は「この daemon は
別の workspace を serve している」から「この workspace は別の daemon が所有している（pid N）」へ変わる。

`ServerHello` は接続が解決した tenant の root を返す。client 側の fence 検証（自分が申告した root と daemon が
返した root の一致）は現行のまま機能する。

### state layout

data directory の layout は**ほとんど変えない**。tenant ごとに分けるのは workspace lifecycle 文書だけである。

| 対象 | scope | 理由 |
|---|---|---|
| `sessions.json` | **workspace ごと**（`<data-dir>/daemon/w/<digest>/sessions.json`） | `repository_root` と root worktree id を 1 組しか持てない文書であり、tenant ごとに単一書き手を保つ |
| locator（`current.json`）・`bootstrap.lock`・`daemon.json`・単一インスタンス lock | data directory 単位のまま | daemon は machine あたり 1 つのままなので分ける理由が無い |
| generation registry・allocator・`shards/<generation>` | data directory 単位のまま | generation は process の incarnation であって workspace の属性ではない。terminal の所属は `TerminalRef.workspace_id` が既に持つ |
| dispatch registry・inbox・PR inventory | data directory 単位のまま | key は globally unique な typed ID であり衝突しない。列挙 API だけ workspace で絞る |
| retention | 変更なし | 既に workspace ごとの usage と daemon 全体 aggregate を持つ |

digest は [bootstrap broker key](../05-daemon.md#sandbox-bootstrap-broker) と同じ作り（domain separation tag と
length prefix 付き SHA-256 の先頭 6 byte を hex 12 文字）にし、subtree に `root.json`（canonical root）を置いて
**必ず検証**する。一致しなければ suffix を伸ばして次の候補を見る（短縮 digest の衝突で別 workspace の state を
書かないため）。socket path はこの subtree を経由しないので、`sun_path` の長さ予算に影響しない。

legacy layout（`<data-dir>/daemon/sessions.json`）からは、初回起動時に `repository_root` の subtree へ
rename で一方向移行し、`runtime-migration.json` と同じ形で記録する。

### 資源の上限

tenant が増えても daemon 全体の資源が線形に増えないよう、既存の 2 段集計を使う。

| 上限 | 値の置き場所 |
|---|---|
| workspace ごとの terminal / final retention | 既存の retention（workspace ごとの usage） |
| daemon 全体の terminal / final retention | 既存の retention（daemon aggregate） |
| adopt 済み tenant 数 | 新規。上限到達時は typed refusal（利用者には「使っていない workspace を retire するか、daemon を再起動する」を提示） |
| 同時 adopt | 新規。1 workspace につき 1 回に直列化する |

### CLI と TUI

| 面 | 変更 |
|---|---|
| `usagi daemon status` | adopt 済み tenant を列挙する（root・session 数・live runtime 数）。`--workspace` のような対象指定は不要 |
| `usagi daemon stop` / `restart` / `replace` | 対象は machine の daemon 1 つのまま。`stop` の live runtime 拒否（`--force`）は全 tenant を対象に判定する |
| `usagi daemon retire <path>` | 段階 5 で追加。tenant 1 件だけを解放し、daemon は動き続ける |
| `usagi daemon install-service` | supervisor が pin する workspace は **initial tenant** の意味になる。以後の workspace は adopt で増える |
| TUI Welcome の Open / Recent | 別 workspace を選んでも refusal にならず、そのまま開く |

TUI の [workspace の離脱と終了](../03-tui.md#workspace-の離脱と終了)は変えない。1 process が持つ daemon 接続は
1 本のままで、離脱時に前 workspace の port・pump・worker を落としてから開き直す。**接続先の daemon は同じ**に
なるので、切り替えは handshake の再実行だけで済む。

## 変えないもの

| 対象 | 理由 |
|---|---|
| workspace fence の invariant（1 canonical workspace root の所有者は machine で 1 つ） | git worktree・branch・session 名という物理資源の所有権。daemon が N 個の fence を持つのは所有権の多重化ではなく、所有者が 1 つのままで対象が増えることである |
| runtime mode による data 分離 | mode は data を分ける規則であり、workspace の所有権は分けない（[13](13-daemon-singleton-and-teardown.md)） |
| 1 process 1 daemon 接続（TUI） | pane・pump の所有者を曖昧にしないため |
| generation・rollover の機構 | generation は process の incarnation であり、tenant 次元を持ち込まない |
| refusal path と test | fence が他 process に握られている場合に必要。削除しない |

## 受け入れるコスト

| コスト | 内容 | 緩和 |
|---|---|---|
| 障害の波及 | 主経路の panic は `catch_unwind` の外で process 終了になり、crash / cold restart は**全 tenant の PTY** を巻き込む | build 入れ替えの日常経路は seamless rollover が PTY を保つ。crash からの復帰は [16. restart 後の状態復帰](16-restart-state-restoration.md)、PTY の物理的継続は [07. PTY crash 継続](07-pty-crash-continuation.md) が担う。本提案はこの 2 提案の価値を上げる |
| tenant 境界で隔離しない | 1 tenant の unwind を catch して他 tenant を生かす形は取らない。unwind 後に daemon 内部 state の整合を保証できないため | 復帰の単位は tenant ごとの durable state（`sessions.json`・durable operation）に置く |
| 秘密情報の同居 | 全 tenant の解決済み env / secret が 1 process に載る | 現行も 1 workspace 分は同居している。env の解決キャッシュは既に workspace ごとの key を持つ。tenant をまたぐ cache 共有を作らないことを test で固定する |
| adopt の失敗経路が増える | 「開こうとした workspace だけが拒否される」状態が新たに起こる | 拒否は workspace 単位の typed refusal として提示し、他 tenant の表示・runtime に影響させない |

## 却下した代替案

| 案 | 却下理由 |
|---|---|
| **workspace ごとに daemon process を立てる**（本書の初版） | daemon の中身はほぼ無変更で済む代わりに、client 側が複雑になる。workspace digest の subtree、`root.json` の最長一致による cwd 解決、`sun_path` 104 byte の長さ予算、generation socket sweep の分離、`daemon status/stop --workspace|--all`、workspace ごとの supervisor install がすべて必要になる。加えて domain が既に持つ workspace 次元を **process 分割で二重に表現する**ことになり、横断ビューの道を塞ぐ |
| 現状維持（refusal の文面改善だけ） | Welcome の Open / Recent は登録済み workspace を全件出すのに 1 つしか開けない。切り替えのたびに `daemon stop` が要り、live Agent を持つ daemon の stop は `--force`（= 実行中の Agent を捨てる）になる |
| workspace ごとに `$USAGI_HOME` を分ける（現在の回避策） | 動くが、`workspaces.json`（Recent）・`settings.json`（global 設定・env）・logs・agent state まで割れる。すべての shell で env を正しく設定し続ける規律も要求する。**回避策として案内するが、製品の答えにはしない** |
| TUI が切り替え時に自動で stop → start する | live runtime があれば結局拒否になり、無ければ他 workspace の Agent を落とす |
| tenant ごとに child process を持つ（daemon が supervisor になる） | 障害の隔離は得られるが、PTY・generation・allocator の所有が親子に分かれ、rollover と custody の議論をすべて 2 階層でやり直すことになる。隔離の価値は [07](07-pty-crash-continuation.md) の PTY broker で別途取りにいくほうが安い |

## 段階と issue 分割

| 段階 | issue | 内容 | 独立して出荷できるか |
|---|---|---|---|
| 1 | #708 | `sessions.json` を `w/<digest>/` の tenant 文書へ分離し、legacy layout から一方向 migration する。tenant は 1 つのままで挙動不変 | 可 |
| 2 | #709 | tenant registry と `FileWorkspaceFence` の多重保持、on-demand adopt と graceful な retire。IPC からはまだ initial tenant だけを使う | 可 |
| 3 | #710 | handshake admission の tenant 解決（`selected` は完全一致、`bound` は最長一致、未 adopt は adopt）、tenant 上限、`daemon status` の tenant 一覧 | 可（ここで CLI / MCP が別 workspace から使える） |
| 4 | #711 | TUI: Open / Recent からの切り替えを refusal ではなく open にする。実 PTY E2E。正本（[05-daemon.md](../05-daemon.md) / [04-ipc.md](../04-ipc.md) / [03-tui.md](../03-tui.md)）の畳み込み | 可（利用者に見える価値が出る） |
| 5 | #712 | 遊休 tenant の retire と `usagi daemon retire <path>` | 可 |

## test 戦略

| 層 | 検証 |
|---|---|
| unit | digest の決定性と `root.json` 不一致時の probing、tenant registry の adopt / retire、fence 取得失敗が **その workspace だけ**を拒否すること、`bound` の最長一致（session worktree からの解決を含む）、tenant 上限 |
| integration（daemon） | 2 つの fixture workspace を 1 daemon が同時に serve し、session 一覧・scope・terminal が混ざらない。片方の tenant の session 作成が他方の `sessions.json` を書かない。fence を先に別 process が握った workspace だけが refusal になる。legacy layout からの migration が正しい subtree に着地する |
| integration（root） | `daemon status` が両 tenant を列挙する。`stop` は live runtime を持つ tenant があれば `--force` を要求する。env / secret の解決キャッシュが tenant をまたがない |
| E2E（実 PTY） | workspace A を開いて Agent を live にしたまま Welcome へ戻り、workspace B を開いて操作し、A の Agent が生存していることを確認する（[重い E2E の直列化](../06-conventions.md#重い-e2e-の直列化)の列に載せる） |
