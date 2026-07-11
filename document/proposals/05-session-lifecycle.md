# 提案: session lifecycle の永続化

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md)

session の作成・初期化・削除・失敗を `state.json` の永続 lifecycle として一元管理し、TUI、CLI、MCP、daemon が
同じ状態から表示と操作可否を判断するための設計提案である。本書は未実装部分を含むため、現在仕様の正本ではない。
現在の session 保存形式は [workspace データ](../data/02-workspace.md#セッションごとsessionrecord)、現在の TUI 表示は
[ホーム画面のサイドバー](../design/home/03-sidebar.md#新規セッション行-new-session)を参照する。

## 目次

- [設計目標](#設計目標)
- [状態名と責務境界](#状態名と責務境界)
- [状態遷移](#状態遷移)
- [永続形式](#永続形式)
- [操作プロトコル](#操作プロトコル)
- [クラッシュ回復と reconcile](#クラッシュ回復と-reconcile)
- [表示モデル](#表示モデル)
- [consumer ごとの方針](#consumer-ごとの方針)
- [失敗の表現と回復操作](#失敗の表現と回復操作)
- [互換性と移行](#互換性と移行)
- [段階的導入](#段階的導入)
- [検証計画](#検証計画)
- [設計判断](#設計判断)

## 設計目標

- session の存在、利用可否、作成・削除処理中かどうかの正本を `state.json` に一本化する。
- 同じ workspace を開く複数の TUI、CLI、MCP、daemon が同じ lifecycle を観測する。
- `(workspace root, session name)` ごとに画面上の行を常に一つだけにし、loading 行と通常行を別の情報源から重ねない。
- 長い Git・filesystem・setup 処理中に workspace 全体の state lock を保持しない。
- process の強制終了、遅延した worker 完了、同名操作の競合から永続状態が収束する。
- 旧形式の `state.json` は移行でき、未知形式や未知 lifecycle を通常 session として誤操作しない。

この設計で表示は一定になる。ただし条件は、**lifecycle を保存するだけでなく、サイドバー行の情報源を
`state.json` だけにすること**である。TUI のローカル `pending_sessions` と永続 record の両方が行を生成するままでは、
status を追加しても二重表示は残る。

## 状態名と責務境界

永続キーは `status` ではなく `lifecycle` とする。`status` は既存の Git `BranchStatus` や手動ラベルと意味が重なるためである。

| ユーザー向けの概念 | 永続値 | 意味 |
|---|---|---|
| loading / 作成中 | `creating` | 名前を予約済みで、worktree・コピー・submodule・skill link を構築中 |
| init / 初期化中 | `initializing` | session の物理構造は完成し、`setup_commands` を実行中 |
| active / 通常利用 | `available` | session を通常利用できる |
| 削除中 | `deleting` | 削除意図を保存済みで、pane・worktree・branch・補助データを削除中 |
| error | `failed` | lifecycle 操作が完了できず、明示的な回復操作を待っている |
| 削除済み | record 不在 | 物理削除と後始末が完了し、`state.json` から除去済み |

`active` は永続 lifecycle 名に使わない。session が「利用可能」でも pane や Agent が動いていない場合があり、既存の
`last_active`、live pane、`AgentPhase` の active 判定とも衝突する。`ready` も `AgentPhase::Ready` と重なるため、通常状態は
`available` とする。

状態の軸は次のように分離する。

| 型 | 問い | 例 |
|---|---|---|
| `SessionLifecycle` | session 実体を利用できるか、構築・削除・回復中か | `creating`, `available`, `failed` |
| `AgentPhase` | session 内の Agent が現在何をしているか | `ready`, `running`, `waiting`, `ended` |
| `BranchStatus` | worktree と統合 branch の Git 関係 | `dirty`, `local`, `pushed`, `synced` |
| manual label | ユーザーが付けた作業分類 | todo, doing, review, done |

## 状態遷移

```text
record 不在
    │ create
    ▼
 creating ── setup あり ──▶ initializing
    │ setup なし                 │ success
    └──────────────┬─────────────┘
                   ▼
               available ── remove ──▶ deleting ── success ──▶ record 不在

 creating / initializing / available / deleting ── failure / integrity ──▶ failed
 failed ── retry / continue / cleanup ──▶ 対応する lifecycle
```

許可する遷移は次のとおり。

| 遷移元 | 遷移先 | 条件 |
|---|---|---|
| record 不在 | `creating` | 名前の形式検証後、同名 record が無いことを lock 下で確認して予約 |
| `creating` | `initializing` | 全 worktree・コピー・submodule・skill link の構築と状態検査が成功し、setup がある |
| `creating` | `available` | 物理構築が成功し、実行する setup が無い |
| `creating` | `failed(create)` | 構築失敗、process 中断、整合性検査失敗 |
| `creating` | `deleting` | cooperative cancel を受理し、作成済みの部分を掃除する |
| `initializing` | `available` | 全 setup command が成功 |
| `initializing` | `failed(initialize)` | command 失敗または process 中断 |
| `initializing` | `deleting` | cooperative cancel 後に session を掃除する |
| `available` | `deleting` | dirty check とユーザー確認を通過し、削除を開始 |
| `available` | `failed(integrity)` | 必須 worktree の消失など、利用可能という invariant が崩れた |
| `deleting` | record 不在 | pane、worktree、branch、補助データをすべて削除済み |
| `deleting` | `failed(delete)` | 一部削除、branch 残存、process 中断 |
| `failed(create)` | `creating` / `deleting` | 再試行 / 残骸の破棄 |
| `failed(initialize)` | `initializing` / `available` / `deleting` | 明示的な再試行 / このまま利用 / 削除 |
| `failed(delete)` | `deleting` | 通常または force で削除を再試行 |
| `failed(integrity)` | `available` / `deleting` | 再検査で復旧を確認 / 削除 |

`setup_commands` は非冪等になり得るため、中断後に自動再実行しない。現行仕様では command 失敗をログだけに残して
session を通常利用可能にするが、本設計では自動起動を壊れた初期環境へ進めないため `failed(initialize)` にする。
現行どおり残りの command も順に試行した後、1 件でも失敗していればこの状態へ進む。ユーザーは失敗内容を確認し、
再試行するか「このまま利用」で `available` へ明示的に進める。

## 永続形式

### workspace state envelope

`state.json` 専用の versioned envelope を導入する。既存の共通 envelope は `version: u32` を読みながら値を検証しないため、
session lifecycle の互換性境界には使わない。

```json
{
  "format": "usagi-workspace-state",
  "version": { "major": 2, "minor": 0 },
  "revision": 42,
  "sessions": [],
  "updated_at": "2026-07-12T07:00:00Z"
}
```

- `major` が未知なら state 全体を fail-closed で読み取り専用エラーにする。
- `minor` は追加フィールドだけの互換変更で上げる。ただし reader が対応する値より新しい `minor` は、未知フィールドを
  round-trip で保持できる実装が入るまでは読み取り専用にする。初期実装は未知値を捨てて上書きしない。
- `revision` は lock 下の保存ごとに単調増加させる。TUI は workspace ごとの最終適用 revision より古い非同期 snapshot を捨てる。
- `updated_at` の既存用途は変えず、個々の lifecycle 更新時刻は `lifecycle.changed_at`、保存順序は `revision` で表す。
- 旧 binary は `version: u32` を期待するため、object 形式の v2 を parse できず fail-closed になる。未知 lifecycle を無視して
  `available` と誤認するより、再起動・更新を要求する方が安全である。

### SessionRecord.lifecycle

すべての v2 session record に lifecycle と session incarnation を明示する。旧 v1 record は自動で `available` と解釈せず、
[互換性と移行](#互換性と移行)の quiescence 確認を終えてから変換する。v2 で未知の `state` は `available` に縮退させず、
state 全体の読み込みを安全側に失敗させる。

```json
{
  "name": "feature-x",
  "session_id": "019a-session-incarnation-id",
  "root": "/repo/.usagi/sessions/feature-x",
  "worktrees": [],
  "created_at": "2026-07-12T07:00:00Z",
  "lifecycle": {
    "state": "creating",
    "attempt": 1,
    "operation_id": "019a-session-operation-id",
    "changed_at": "2026-07-12T07:00:00Z",
    "progress": { "completed": 0, "total": 2 }
  }
}
```

`lifecycle` は state 固有データを持てる tagged object にする。

| フィールド | 型 | 意味 |
|---|---|---|
| `state` | enum | `creating` / `initializing` / `available` / `deleting` / `failed` |
| `attempt` | u64 | 同じ session incarnation 内で lifecycle 操作を開始・再試行するたび増える fencing 世代 |
| `operation_id` | string? | 実行中または失敗した一回の操作を識別する opaque ID。`available` では省略 |
| `changed_at` | RFC3339(UTC) | 最後に意味のある lifecycle 遷移を保存した時刻。liveness 判定には使わない |
| `progress` | object? | repository / setup step の完了数。animation frame や人間向け文言は保存しない |
| `setup_plan` | object? | `initializing` / `failed(initialize)` で保持する setup の immutable snapshot と実行 cursor |
| `delete_plan` | object? | `deleting` / `failed(delete)` で保持する cleanup 対象 snapshot と force 方針 |
| `failure` | object? | `failed` で必須となる安全な失敗概要 |
| `cancel_requested` | bool? | 作成・初期化 worker への cooperative cancel。既定 `false` |

`session_id` は record 作成時に発行する opaque UUID で、record が存在する間は不変とする。同じ名前を削除後に再作成した場合は
必ず新しい値になる。これにより、削除前の遅い worker が同名の新 session を更新することを防ぐ。`created_at` は名前を予約して
`creating` record を作った時刻とする。`worktrees` は `creating` 中は空または構築済みの部分集合、
`initializing` / `available` では完成済み一覧、`deleting` / `failed(delete)` では削除開始前の snapshot を保持する。

状態固有 field の invariant は domain constructor と deserialize 後の validation の両方で検査する。

| state | 必須 | 禁止または除去 |
|---|---|---|
| `creating` | `attempt`, `operation_id`, `progress` | `failure`, `setup_plan`, `delete_plan` |
| `initializing` | `attempt`, `operation_id`, `setup_plan` | `failure`, `delete_plan` |
| `available` | `attempt` | `operation_id`, `failure`, `setup_plan`, `delete_plan`, `cancel_requested` |
| `deleting` | `attempt`, `operation_id`, `delete_plan` | `failure`, `setup_plan`, `cancel_requested` |
| `failed` | `attempt`, `operation_id`, `failure` | failure stage と無関係な plan、`cancel_requested` |

`failed(initialize)` は `setup_plan`、`failed(delete)` は `delete_plan` を追加で必須とする。`failed(create)` は構築済みの
`worktrees` と `progress`、`failed(integrity)` は検査時の record snapshot から回復する。invariant 違反の v2 record は
部分的に推測せず workspace state 全体を読み取り専用エラーにする。

### setup plan と実行 cursor

`creating` から `initializing` へ移る一回の保存で、その時点の `setup_commands` を `setup_plan.commands` へコピーする。
設定が後から変わっても、同じ session incarnation の続行・再試行はこの snapshot を使う。展開済み環境変数、stdout、stderr は
保存しない。

```json
{
  "setup_plan": {
    "commands": ["cargo fetch", "cargo check"],
    "pending_indices": [1],
    "running_index": null,
    "outcomes": [
      { "index": 0, "status": "succeeded", "attempt": 1 }
    ]
  }
}
```

初回は全 index を `pending_indices` に入れる。各 index は常に pending、running、outcome のどれか一つだけに属する。
command の起動直前に先頭 index を pending から `running_index` へ移して保存し、終了直後に running を
`succeeded` / `failed` outcome へ移して保存する。outcome は index ごとの最新結果だけを保持し、履歴は trace log へ送る。
失敗後も現行仕様どおり残る pending を順に試し、最後に一件でも非 `succeeded` outcome があれば `failed(initialize)` へ進む。

process が `running_index` を残して終了した場合、その command は「実行されたが結果未記録」かもしれない。reconciler はこれを
`ambiguous` outcome へ移して `failed(initialize)` にし、自動再実行しない。回復画面で当該 step の再試行、ambiguous のまま
残る pending だけを続行、または不完全な状態を `available` として受容する操作を明示的に選ばせる。

retry は `attempt` と `operation_id` を更新し、成功済み outcome を維持したまま、ユーザーが選んだ failed / ambiguous index の
outcome を除いて ordered pending へ戻す。既存 pending だけを続行する場合も新 attempt とし、`commands` 自体は変えない。
別内容へ置換する場合は変更前後を trace に残して新 plan の全 index を pending にする明示操作とし、通常 retry と区別する。

### delete plan

`deleting` への遷移時に、session ID、worktree path と branch、pane / queue / 補助 store の識別子、force 方針を
`delete_plan` へ snapshot する。cleanup はこの plan の各 target を「無ければ成功」として冪等に処理し、現在の設定や同名 directory の
再探索で対象を増やさない。各 path が許可された workspace / session root 内にあることを再検証してから削除する。

## 操作プロトコル

### lock と fencing

意味上の正本は `state.json`、lock は競合と process 生存の判定にだけ使う。

```text
.usagi/.lock                              # 短い state.json read-modify-write
.usagi/session-operations/<name>.lock    # 同名 session の長時間操作を排他
.usagi/session-tree.lock                 # worktree/branch topology の変更を直列化
.usagi/state-migration.lock              # v1 → v2 の一回限りの移行を排他
```

- session operation lock は作成・初期化・削除の全期間保持する。process crash では OS が advisory lock を解放する。
- session tree lock は worktree / branch を構築・削除する区間だけ保持し、任意長の setup 中は解放する。別名 session の setup と
  worktree 構築を並行できる。
- workspace state lock は各 lifecycle 遷移の load → compare → save だけで保持する。state file 自体の read / atomic save / backup
  以外の Git、session filesystem、shell command を実行しない。
- state lock を保持したまま他の lock を待たない。各完了更新は
  `(name, session_id, expected state, attempt, operation_id)` が一致するときだけ
  適用し、古い worker の遅延完了は no-op としてログへ記録する。

### create

1. session operation lock を取得する。
2. 名前形式を検証し、state lock 下で同名 record が無いことを確認する。
3. `creating` record と新しい `session_id` / `attempt` / `operation_id` を保存して名前を予約する。
4. state lock を解放し、session tree lock 下で対象名の古い stray を掃除して物理構築する。
5. 有意な単位が完了したときだけ `progress` と部分 `worktrees` を条件更新する。
6. 構築完了後、setup があれば `initializing`、無ければ `available` を保存する。
7. `initializing` では session tree lock を解放したまま setup を順に実行する。
8. 全成功で `available`、失敗があれば `failed(initialize)` を保存して operation lock を解放する。

入力形式エラーは record を作らず即時に返す。予約後に判明した Git・filesystem・setup の失敗は record を消さず `failed` にし、
全 process から同じ失敗と回復入口が見えるようにする。

### remove

1. session operation lock を取得し、対象が `available` または回復可能な `failed` であることを確認する。
2. non-force では dirty check と確認を先に行う。拒否は lifecycle を変えずに返す。
3. state lock 下で `deleting` と `operation_id`、cleanup 対象の immutable `delete_plan`、force 方針を保存する。
4. state lock を解放し、session tree lock 下で pane snapshot、worktree、branch、コピー、session 単位の補助 store を冪等に掃除する。
5. **すべての後始末が成功した後だけ** state lock 下で record を除去する。
6. 一部でも残れば `failed(delete)` を保存する。branch 削除失敗をログだけにして record を消さない。

作成・初期化中の cancel は `cancel_requested` を同じ operation ID に条件更新する。worker は repository / setup step の境界で確認し、
受理後は `deleting` へ遷移して作成済み部分を掃除する。shell command の強制 kill は別設計とし、command 実行中は
「キャンセル待ち」を表示する。

## クラッシュ回復と reconcile

時間経過だけで「遅い処理」と「死んだ処理」を区別しない。reconciler は lifecycle を変更する前に対象の session operation lock を
non-blocking で取得し、取得できなければその tick では何もしない。非終端 record は取得後の再読み込みでも同じ `operation_id` だった
場合だけ abandoned operation と判定する。`available` の integrity 検査も lock を保持して実行し、再読み込みで同じ
`session_id` / `attempt` / `available` を確認してから、新しい attempt / operation ID とともに `failed(integrity)` を条件保存する。
これにより prompt / spawn reservation や remove の開始と integrity 遷移を同じ排他境界に置く。

| 観測状態 | 物理状態 | 回復 |
|---|---|---|
| `creating` | 任意 | `failed(create, interrupted)`。部分 worktree を自動再実行・自動削除しない |
| `initializing` | worktree 完成 | `failed(initialize, interrupted)`。非冪等 setup を自動再実行しない |
| `deleting` | 全リソース消失 | record を除去して削除完了へ収束 |
| `deleting` | 一部残存 | 保存済み delete intent に従い冪等 cleanup を一度再開し、失敗なら `failed(delete)` |
| `available` | 必須 worktree 欠落 | `failed(integrity)`。通常起動を止める |
| `failed` | 任意 | 自動遷移しない。ユーザーまたは明示 policy の回復操作を待つ |

従来の stray reconcile は **record の無い directory だけ**を対象にする。`creating` や `failed(create)` の部分 directory は
lifecycle record が所有しているため、汎用 orphan cleanup が勝手に削除しない。

reconcile の実行契機は TUI 起動、daemon tick、create/remove の前処理とする。同じ `operation_id` に対する回復は冪等にし、
二つの process が同時に回復しようとしても session operation lock と条件更新で一方だけが進む。

## 表示モデル

TUI は workspace snapshot の session record を `(canonical workspace root, name)` で一行へ射影する。

| lifecycle | サイドバー | 選択・操作 |
|---|---|---|
| `creating` | 青系 skeleton、「作成中」、任意の粗い進捗 | 詳細 / cancel のみ。terminal・Agent・rename・reorder 不可 |
| `initializing` | 青系 skeleton、「初期化中」、setup step | 詳細 / cancel のみ。自動起動不可 |
| `available` | 現在の通常 session 行 | 通常操作可。AgentPhase と Git status を重ねる |
| `deleting` | 同じ位置の赤系 skeleton、「削除中」 | 詳細のみ。新規 pane・編集・重複 delete 不可 |
| `failed(create)` | 静的 error 行、「作成失敗」 | 詳細 / retry / 残骸削除 |
| `failed(initialize)` | 静的 error 行、「初期化失敗」 | 詳細 / retry / このまま利用 / recovery terminal / delete |
| `failed(delete)` | 静的 error 行、「削除失敗」 | 詳細 / retry / force delete |
| `failed(integrity)` | 静的 error 行、「整合性エラー」 | 詳細 / 再検査 / 存在確認済み worktree の recovery terminal / delete |
| record 不在 | 行なし | 操作不可 |

sidebar の行生成には TUI ローカルの `pending_sessions` を使わない。操作直後は state 保存が返した revision 付き snapshot を
その TUI に即時適用し、file watcher は同じ record の後続 revision を届ける。animation frame は各 TUI が `changed_at` と現在時刻から
ローカルに計算し、`state.json` を毎 frame 更新しない。

`TaskHandle` は worker thread、完了ログ、TUI 起点だけの auto-focus に残してよいが、session 行の有無や lifecycle の正本にはしない。
worker 完了と watcher refresh の到着順が逆でも、revision と operation ID により古い表示へ戻らない。
v2 cutover までの現行実装が行う「ローカル pending と先着した record の重複抑止」は互換 bridge とし、cutover 後に
永続 lifecycle からの一行生成へ置き換える。

availability に依存する prompt queue 書き込み、autostart claim、pane spawn 予約は、session operation lock を短く取得して
`available` を再確認し、queue / reservation の commit まで終えてから解放する。state を先に読んだだけでは、その直後に
`deleting` が始まる check-to-use race を防げない。削除処理は lock 取得後に committed queue と spawn reservation を掃除し、
新しい処理が対象 session へ入らない境界を作る。

## consumer ごとの方針

各 consumer が enum を独自に match しないよう、domain に `SessionCapabilities` を置き、`can_launch`、`can_remove`、
`is_syncable`、`protects_disk`、`counts_as_worker` などを一か所で導出する。

| consumer | 方針 |
|---|---|
| TUI / workspace overview | 全 lifecycle を一行ずつ表示。通常 focus / switch 候補は `available` だけ |
| `session list` / `session status` | 全 record と lifecycle、失敗工程を返す。空 `worktrees` を ready / merged と判定しない |
| terminal / Agent / pane restore | `available` だけ通常起動。`failed(initialize)` の terminal は明示的 recovery action のみ |
| `session_prompt` / queued prompt / autostart | `available` だけ配送・起動。その他は lifecycle 付き待機または明示エラー |
| create / delegate | 同名 record は lifecycle にかかわらず名前予約済み。重複 worker を作らない |
| remove | `available` と回復可能な `failed` を対象にする。実行中 operation には cancel / retry policy を適用 |
| metadata 編集 | note / todo / decision の閲覧は全状態、通常編集は `available` と `failed(initialize)` に限定 |
| Git sync / update | 完成した worktree を持つ `available` を通常対象にし、failed record は工程に応じて検査する |
| reconcile | 全 record が directory を所有する。非終端の dead operation と物理不整合を回復する |
| daemon snapshot | lifecycle を含める。runtime activity / AgentPhase と混ぜない |
| durable orchestrator | `creating` / `initializing` / `deleting` も worker の存在・名前予約として数える。`failed` は失敗観測 |

durable orchestrator と status 判定では特に、空 `worktrees` に対する `all(...) == true` を避ける。merged / ready 判定は
`lifecycle == available` を前提にし、非 `available` record を単に一覧から落として「session 不在」と誤認しない。

## 失敗の表現と回復操作

`failed` は工程を必須にし、`create` / `initialize` / `delete` / `integrity` の回復方法を区別する。

```json
"lifecycle": {
  "state": "failed",
  "attempt": 2,
  "operation_id": "019a-session-operation-id",
  "changed_at": "2026-07-12T07:01:00Z",
  "failure": {
    "stage": "initialize",
    "code": "command_failed",
    "summary": "2 setup commands failed",
    "error_id": "20260712-abc123",
    "retryable": true
  }
}
```

- `summary` は画面・MCP に出せる短い安全な文言だけを保存する。
- shell stderr、panic payload、secret を含み得る詳細は既存 error / trace log に `error_id` で保存する。
- retry 成功時は lifecycle の `failure` を消す。履歴は trace log に残し、state record に無制限蓄積しない。
- retry は `attempt` を増やして新しい `operation_id` を発行する。`session_id` は維持する。
- `continue` は `failed(initialize)` だけに許可し、ユーザーが不完全な初期化を受容した事実を trace に残す。
- `failed(integrity)` は通常 terminal や Agent を起動しない。再検査で invariant が回復していれば `available`、回復不能なら
  `deleting` へ進める。調査用 terminal は、選択した worktree path が record 内にあり実在することを再確認した明示操作に限る。

## 互換性と移行

現行 v1 は session record を setup 完了前にも保存し得るため、field 欠落を一律 `available` とみなすと移行の瞬間に構築途中の
session を通常起動できてしまう。また旧 process は長時間処理中に workspace lock を保持し続けないため、lock が空いていることだけでは
quiescence を証明できない。このため v1 → v2 は最初の通常 mutation に便乗させず、明示的な migration barrier にする。

1. v2 reader は現行 `version: 1` を legacy snapshot として読み、永続 lifecycle とは別の `migration_pending` projection で
   read-only 表示する。create / remove / Agent 起動などの mutation は止め、再起動案内を出す。
2. ユーザーは同じ workspace を扱う旧 TUI、daemon、MCP、CLI の長時間操作を終了し、全 process を閉じてから
   `usagi doctor --migrate-workspace-state`（仮称）を実行する。v1 を安全に自動判別できない skipped upgrade では、この確認を省略しない。
3. migration は state migration lock を取得し、state lock 下で v1 bytes と digest を読み取ってから state lock を解放する。
   宣言された worktree / branch / path の物理 invariant は lock 外で検査する。
4. 検査後に state lock を取り直して v1 を再読込し、bytes / digest が一致しなければ検査結果を捨てて最初からやり直す。
   quiescence の明示確認済みかつ検査成功の record だけに新しい `session_id` と `available` を付与し、不整合 record は
   `failed(integrity)` にする。state lock 下では Git command や対象 path の検査を行わない。
5. v2 save 前に一度だけ `state.v1.backup.json` を atomic write する。backup を作れなければ cutover を中止し、v1 を維持する。
6. backup 完了後、同じ state lock 区間で `revision = 1` の v2 を atomic save する。途中失敗時は v1 または完全な v2 の
   どちらかだけが残る。
7. 旧 binary は v2 の object version を parse できず、再読込時の mutation を拒否する。互換 release で process-lifetime marker を
   先行導入できる場合は自動 quiescence 判定に使えるが、旧 binary が state snapshot を保持したまま後書きしないよう、cutover の
   運用条件として旧 process 終了を必須にする。
8. 未知 major / minor / lifecycle は fail-closed。ユーザーへ「usagi を更新し、古い process を終了する」案内を出す。

共通 `json_file::FILE_FORMAT_VERSION` は settings や index にも使われるため変更せず、`WorkspaceStore` だけが専用 envelope を扱う。

## 段階的導入

| 段階 | 内容 | cutover 条件 |
|---|---|---|
| 1. reader / domain | v2 parser、`SessionLifecycle`、capability、revision 付き snapshot を dormant code として追加。production reader / writer は従来の v1 | legacy migration と未知値 fail-closed のテスト |
| 2. operation core | transition、operation ID、session / tree lock、crash injection と reconcile を cutover flag の背後へ実装 | dormant path の全 crash point が `available` / `failed` / record 不在へ収束 |
| 3. consumer | TUI、CLI、MCP、daemon、autostart、orchestrator の capability 経路を同じ flag の背後へ実装 | dormant path の consumer matrix characterization test |
| 4. atomic cutover | migration barrier と v2 writer、operation core、全 consumer を同時に有効化し、サイドバー行から `pending_sessions` を除去 | 複数 TUI で一 record = 一行、旧 process が fail-closed |
| 5. 正本化 | data / orchestration / command / design 文書へ実装済み仕様を移し、本提案をリンク stub 化 | 正本と実装が一致 |

段階 1〜3 の production 経路は現行 v1 reader / writer / consumer のままとし、v1 を `migration_pending` にする reader も有効化しない。
内部の非ユーザー設定 cutover flag は個別に有効化できないようにする。reader だけ、enum だけ、または一部 consumer だけを先に
v2 保存・利用しない。create の予約を永続化する release には、migration barrier、stale operation の検出、`failed` への回収、
回復削除、全 consumer の capability gate を同時に含める。

## 検証計画

### domain / persistence

- 許可遷移の全組合せと、不正遷移が state を変えないこと。
- session ID / attempt / operation ID が異なる遅延完了を拒否し、同名再作成後の旧 worker が更新できないこと。
- v1 migration pending の mutation 拒否、明示 quiescence 後の v1 → v2、backup 失敗、v2 round-trip、未知 major / minor / lifecycle の
  fail-closed。
- setup command の起動前後 crash と ambiguous `running_index` が自動再実行されず、同じ plan から明示回復できること。
- revision の単調増加と、並行 writer の lost update 防止。

### crash / concurrency

- create の予約後、各 worktree 後、`initializing` 保存前後、各 setup 後、`available` 保存直前で process を中断する。
- delete の `deleting` 保存後、pane cleanup 後、各 worktree / branch 後、record 除去直前で中断する。
- live operation lock は回収せず、解放済み lock と同じ operation ID だけを回収する。
- 同名 create/create、create/remove、remove/remove は一つだけ進み、別名 session は setup 中に進行できる。
- stale `deleting` の物理削除完了 / 一部残存をそれぞれ record 不在 / `failed(delete)` へ収束させる。

### presentation / consumer

- create → initialize → available と delete → record 不在の各 snapshot で、同名行が常に一つで高さと hit-test が一致する。
- 二つの TUI が同じ revision 列を異なる到着順で受けても古い表示へ戻らない。
- non-available session が switch、autostart、prompt、pane restore の対象にならない。
- 別 workspace の同名 session は lifecycle を共有しない。
- 空 worktrees の `creating` が merged / completed worker と誤判定されない。
- failed stage ごとに提示される recovery action が capability と一致する。

## 設計判断

- **採用**: `state.json` を session lifecycle の唯一の意味状態にする。
- **採用**: `creating / initializing / available / deleting / failed`。削除完了は record 不在。
- **採用**: session operation lock で liveness、session ID / attempt / operation ID で fencing、revision で snapshot 順序を保証する。
- **採用**: `failed` は工程と安全な概要を持ち、詳細ログとは ID で結ぶ。
- **採用**: setup failure は自動起動を止めるが、明示的な「このまま利用」を許す。
- **不採用**: `active` を lifecycle の通常状態にする。runtime activity と区別できない。
- **不採用**: `deleted` tombstone。削除完了は record 不在で十分であり、一覧を無期限に増やす。
- **不採用**: elapsed time / lease だけで crash を判定する。長い正規処理を誤回収し得る。
- **不採用**: TUI-local pending row と永続 lifecycle row の併用。再び二重権威になる。
- **不採用**: lifecycle field だけを additive に追加して旧 reader に無視させる。部分 session を通常利用可能と誤認する。
