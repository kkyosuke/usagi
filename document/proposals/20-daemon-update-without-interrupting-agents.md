# 20. Agent を中断させない daemon update

> [設計提案一覧](README.md) ｜ 関連する現在仕様: [daemon](../05-daemon.md) ｜ [daemon IPC](../04-ipc.md) ｜ [TUI](../03-tui.md) ｜ 関連提案: [restart 後の作業状態復帰](16-restart-state-restoration.md) ｜ [PTY crash 継続](07-pty-crash-continuation.md)

> **Status:** 一部採用済み・一部提案中。planned update の停止 → 引き継ぎ → exact resume は現行ビルドに実装済みで、
> [daemon IPC](../04-ipc.md#provider-conversation-resume-request) が正本である。本書が提案するのは、その経路の
> 語彙・導線・待ち方であり、現在の contract ではない。
>
> **Baseline:** 原版 commit `573dc395df99270f53baaca1bcc4b8d5861b0823`（2026-09-17）。現在仕様は
> [5. daemon](../05-daemon.md#planned-replacement) と [4. daemon IPC](../04-ipc.md#provider-conversation-resume-request) を参照する。

daemon の artifact を入れ替えるとき、利用者が開いている Agent 会話を失わず、かつ事故の語彙である
`interrupted` を見せないための設計である。

## 目次

- [目的と非目標](#目的と非目標)
- [現在地](#現在地)
- [`interrupted` に見えてしまう 4 つの原因](#interrupted-に見えてしまう-4-つの原因)
- [機構](#機構)
  - [M1 planned update の語彙を分ける](#m1-planned-update-の語彙を分ける)
  - [M2 hook が示す静止点で更新する](#m2-hook-が示す静止点で更新する)
  - [M3 update 導線を 1 本にする](#m3-update-導線を-1-本にする)
  - [M4 どの面が旧 build に残っているかを出す](#m4-どの面が旧-build-に残っているかを出す)
- [`--force` と crash をどう扱うか](#--force-と-crash-をどう扱うか)
- [daemon を管理する層は要るか](#daemon-を管理する層は要るか)
- [却下した代替案](#却下した代替案)
- [段階と issue 分割](#段階と-issue-分割)

## 目的と非目標

| | 内容 |
|---|---|
| 目標 | daemon artifact の更新が、開いている Agent 会話を 1 件も失わずに完了する |
| 目標 | 更新中の表示が事故（`interrupted`）と区別でき、tab が同じ位置に留まる |
| 目標 | 実行中の prompt / tool を割り込まずに更新できる |
| 非目標 | crash / `SIGKILL` / 電源断からの PTY 継続（[07](07-pty-crash-continuation.md) の領分） |
| 非目標 | live でないものを live と描くこと。表示を変えて中断を隠すこと |
| 非目標 | 利用者が要求していない更新・resume を自動で始めること |

「引き継ぐ」は **同じ会話 lineage を新しい runtime が継続する**ことであり、旧 process を生き返らせることではない。
planned update ではこの区別が利用者に見えている必要はあるが、事故として見える必要はない。

## 現在地

現行ビルドで既に動いているものは次のとおりで、本書はこれらを作り直さない。

| 経路 | 現在の挙動 | 正本 |
|---|---|---|
| generic terminal だけが live な planned replacement | seamless rollover。old process を draining として残し、その PTY を維持する | [planned replacement](../05-daemon.md#planned-replacement) |
| live Agent がある planned replacement | typed refusal（`mcp_authority_retained`）。old active・PTY・credential は無傷 | [rollover の routing 前提条件](../05-daemon.md#rollover-の-routing-前提条件) |
| `daemon restart --restart-agents` | daemon 全体の live Agent を列挙 → exact resume target を durable transaction へ保存 → 全 Agent 停止 → handoff → **新 build の integration で exact resume** | [provider conversation resume request](../04-ipc.md#provider-conversation-resume-request) |
| `usagi update` | live Agent の credential があれば同期を保留し、`daemon restart --restart-agents` を案内して終わる | 同上 |
| `--restart-agents` なしの `--force` / crash / `SIGKILL` | cold transition。PTY は復元不能で、runtime は `interrupted` として resume 候補に残る | [generation と orphan safety](../05-daemon.md#generation-と-orphan-safety) |

したがって **「daemon を更新しても Agent を引き継ぐ」機能自体は既にある**。欠けているのは、それが既定の導線でなく、
成功しても表示が事故と同じ語彙になり、実行中の Agent がいると `--force` を要求することである。

## `interrupted` に見えてしまう 4 つの原因

| # | 原因 | 影響 |
|---|---|---|
| G1 | 既定の `daemon restart` は live Agent があると refusal になり、利用者は `--force`（cold transition）へ逃げる | 会話 PTY が実際に失われ、`interrupted` が正しく出る |
| G2 | `--restart-agents` の停止は runtime を `Exited` にし、`last_known_phase` に `Interrupted` を書く | 成功経路でも、resume が着地するまでの窓で tab が interrupted / completed 側へ落ちる |
| G3 | `running` phase の Agent がいると `--restart-agents` は `--force` を要求する | 更新のたびに実行中の tool を割り込むか、更新を諦めるかの二択になる |
| G4 | seamless rollover 後、旧 build の draining generation が持つ tab に印が無い | 「更新したのに古い方を触っている」ことに気づけない |

G2 が「interrupted を見せたくない」の中心である。**表示を消すのではなく、planned update と事故に別の語彙を与える**のが
本書の立場である。live でないものを live と描かず、かつ事故ではないものを事故と描かない。

## 機構

### M1 planned update の語彙を分ける

daemon が pending restart transaction（`agent-restart.json`）に載せた runtime は、停止から resume 着地までの間
**`updating`** として投影する。`interrupted`（所有権を証明できなかった事故）とも `exited`（終わった会話）とも別の値にする。

| 層 | 追加 |
|---|---|
| domain | `AgentRuntimeInventoryState::Updating` と `ProviderResumePhase::Updating`。既存 variant の意味は変えない |
| daemon | 停止時に transaction 上の runtime へ `updating` を書き、resume 成功で live へ、transaction 放棄で `interrupted` へ落とす |
| IPC | inventory の closed vocabulary に追加し、未知 variant を受けた旧 client は `interrupted` として安全側に倒す |
| TUI | [interrupted tab 投影](../03-tui.md#interrupted-agent-の-tab-投影と選択時-resume)の前段で `updating` を **同じ slot に留まる live 扱いの placeholder** として描く。label は `Claude (updating)`、body は「この会話は daemon の更新のため再接続中である」 |

`updating` は resume を自動発火させる権限を TUI に与えない。resume を駆動するのは daemon 側の recovery worker であり、
TUI は結果を待つだけである（[明示 resume の検証](../03-tui.md#interrupted-agent-の-tab-投影と選択時-resume)は変えない）。
transaction が失敗・放棄されたときだけ、その lineage は従来どおり `interrupted` の明示 resume 導線へ落ちる。
**嘘をつかないための境界はここである**: `updating` を名乗れるのは durable transaction がその lineage を持っている間だけで、
証明できなくなった瞬間に事故の語彙へ戻る。

### M2 hook が示す静止点で更新する

Agent の lifecycle hook は既に `Stop` → `ended` / `PostToolUse` → `waiting` / `PreToolUse` → `running` を daemon へ報告している。
この報告は既に `--restart-agents` の可否判定（`running` なら `--force` 要求）に使われているので、**待つ**ことにも使える。

`daemon restart --restart-agents --when-idle[=<deadline>]` を加える。

| 段 | 内容 |
|---|---|
| 1 | 更新要求を受けた daemon は新規 Agent launch の admission を止めず、**pending update intent** を持つだけにする（effect 0） |
| 2 | 全 live Agent の報告 phase が `running` でなくなった最初の瞬間に rollover barrier を取り、そこから先は既存の `--restart-agents` 経路と同一である |
| 3 | deadline（既定 15 分）内に静止点が来なければ、effect 0 の typed refusal で終わる。`--force` の意味は変えない |

barrier を取ってから再計算した live Agent 集合が plan と完全一致しない場合に stale として拒否する既存契約はそのまま使う。
静止点の観測は hook 報告に依存するため、hook を持たない provider は従来どおり `--force` を要求する。

### M3 update 導線を 1 本にする

`usagi update` が live Agent を理由に同期を保留する現在の出力は、利用者に別コマンドの再入力を要求している。
保留の代わりに、同じ update lock の中で `--restart-agents --when-idle` 相当を実行するのを既定にする。

- TUI からも同じ操作を出す（daemon status modal の `update` action）。更新対象の Agent 件数と、
  静止点待ちか即時かを事前に示す。
- 自動では始めない。利用者が update を要求した時点で、その resume は「利用者の明示操作の一部」であり、
  [16 の auto-resume 非目標](16-restart-state-restoration.md#目標と非目標)と衝突しない。crash からの復帰だけが明示操作を要求し続ける。

### M4 どの面が旧 build に残っているかを出す

seamless rollover の後、generic terminal は draining generation が持ち続ける。client は既に
[owner generation routing](../04-ipc.md#owner-generation-routing) で owner を知っているので、表示に出せる。

| 対象 | 表示 |
|---|---|
| draining generation が owner の tab | tab label の横に旧 build の印（例 `⟳`）と、選択時の 1 行説明 |
| rollover 進行中 | daemon status modal に active / draining の 2 世代と、draining が持つ terminal 件数 |

印は「古いので壊れている」ではなく「この面だけ旧 build が serve している」という事実である。draining が
[generation collection](../05-daemon.md#generation-と-orphan-safety) で回収されると印は消える。

## `--force` と crash をどう扱うか

`--restart-agents` を伴わない `--force` は、定義上 live runtime を破棄する cold transition である。ここで
`interrupted` を出さないようにすることは**できない**。PTY master fd も child process も失われており、
継続を証明できないからである。本書はこの意味を変えず、代わりに次の 2 つで `--force` を使う理由を無くす。

| 状況 | 本書の後の経路 |
|---|---|
| 更新したいが Agent が実行中 | `--when-idle` が静止点を待つ（M2）。`--force` は不要 |
| 更新したいが Agent が居る | `--restart-agents` が既定（M3）。`--force` は不要 |
| 本当に全部捨てたい | `--force` はそのまま残す。cold transition であると明示し、`interrupted` を正しく出す |
| crash / `SIGKILL` / 電源断 | `interrupted` は事実である。[16](16-restart-state-restoration.md) の restore plan で 1 操作に減らす |

## daemon を管理する層は要るか

「管理者が daemon を rolling update する」形は、**すでに daemon 自身が持っている**。

```text
supervisor（launchd / systemd）   process を生かすだけ。session / Agent の権威を持たない
        │
        ▼
active generation ──[rollover: standby stage → build 検証 → authority commit]──► new active
        │                                                                          
        └─► draining generation（自分の PTY を serve し続け、control と spawn は失う）
```

| 望むもの | 必要な追加 |
|---|---|
| 新 build へ無停止で切り替える | 追加なし。active/standby handoff が既に rolling update である（[planned replacement](../05-daemon.md#planned-replacement)） |
| 落ちたら起こし直す | 追加なし。[service supervision](../05-daemon.md#service-supervision) の launchd / systemd |
| Agent 会話を更新でまたぐ | 本書 M1〜M3。新しい常駐 process は不要 |
| **crash をまたいで同じ PTY に attach し続ける** | 常駐 broker が必要（[07](07-pty-crash-continuation.md)）。data plane を daemon から分離する |

つまり、管理プロセスを足す必要があるのは最後の 1 行だけである。broker は daemon より強い信頼境界（master fd を持つ）に
なるため、着手条件は [07 の実装前提](07-pty-crash-continuation.md#実装前提と-issue-分割)を満たしたときに限る。
本書の機構は broker の有無から独立しており、broker を採っても無駄にならない。

## 却下した代替案

| 代替案 | 却下理由 |
|---|---|
| MCP caller credential を successor へ移送して Agent を止めずに rollover する | credential は `daemon_minted_ephemeral` で、restart が明示的な失効境界である。process 間へ secret を移すと失効境界が消え、旧 active の残存 credential が新 active の authority を名乗れる |
| 新 active が未知 credential を draining generation へ proxy する | draining に control authority を戻すことになる。draining は control / spawn を失っているという契約が rollover 安全性の根拠なので崩せない |
| `interrupted` の label を単に隠す / live として描く | 事故と区別できなくなる。crash 後に「継続した」と描くのと同じ嘘になる |
| `--force` を「可能なら seamless、駄目なら cold」に再定義する | `--force` は利用者が破棄を承認する語であり、承認の意味を状況依存にすると、破棄されたことに気づけない |
| 更新を Agent の終了まで無期限に待つ | 常駐 Agent がいる workspace では永久に更新できない。deadline 付きの静止点待ち（M2）にする |

## 段階と issue 分割

各段は前段までで独立に出荷でき、途中でも既存の `--restart-agents` と明示 resume 契約を壊さない。

| 順序 | issue | 成果物 |
|---|---|---|
| 1 | `feat(core): planned update の runtime state と phase 語彙` | `updating` variant、reducer、未知 variant の安全側 fallback、pure test |
| 2 | `feat(daemon): pending restart transaction を updating として投影する` | 停止 / resume / 放棄の遷移、inventory 投影、transaction 放棄時の `interrupted` 復帰 |
| 3 | `feat(tui): updating tab を同じ slot に保つ` | placeholder tab、label / body、interrupted 投影との優先順位、resume を発火しないこと |
| 4 | `feat(daemon): --when-idle の静止点待ちと deadline` | pending update intent、hook phase の観測、barrier 取得、effect 0 の timeout refusal |
| 5 | `feat(cli+tui): update が既定で Agent を引き継ぐ` | `usagi update` の既定経路、TUI の update action、件数と待ち方の事前提示 |
| 6 | `feat(tui): draining generation が owner の面に印を出す` | tab の印、daemon status modal の 2 世代表示、回収後に消えること |
| 7 | `test(root): live Agent を持つ update の実 PTY E2E` | 実 daemon 2 process と実 PTY で、更新前後の会話 lineage 一致と `interrupted` 非出現を shipping binary で固定する |
