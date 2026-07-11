# 提案: issue DAG 永続オーケストレータ

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md)

複数の依存 issue を一つの統括 session が所有し、worker session を直接生成して進行を継続するための設計提案である。
本書は未実装部分を含むため、現在仕様の正本ではない。現在利用できる機能は
[オーケストレーション](../04-orchestration.md) と [MCP](../03-commands/03-mcp.md) を参照する。

## 目次

- [設計目標](#設計目標)
- [既存機能と不足](#既存機能と不足)
- [基本構造](#基本構造)
- [現状可否](#現状可否)
- [agent skill](#agent-skill)
- [二種類の readiness](#二種類の-readiness)
- [永続状態](#永続状態)
- [reconcile](#reconcile)
- [終端通知](#終端通知)
- [stacked PR](#stacked-pr)
- [失敗と待機](#失敗と待機)
- [競合と安全性](#競合と安全性)
- [段階的導入](#段階的導入)
- [検証計画](#検証計画)
- [設計判断](#設計判断)

## 設計目標

- root または専用の一つの統括 session が issue DAG 全体の唯一の owner になる。
- 統括は worker session だけを直接生成する。worker に再委譲させる多段 tree を通常経路にしない。
- 統括 agent が終了しても、永続状態から同じ判断を再開できる。
- issue の既存 `ready` の意味を変えず、先行 PR の merge 前にも後続の実装を安全に開始できる。
- 常駐プロセスを追加せず、既存 TUI sync、queued prompt autostart、agent phase を wake-up に使う。

## 既存機能と不足

| 能力 | 現在利用できるもの | 不足 |
|---|---|---|
| DAG | issue の `dependson`、`issue graph` | merge 前に作業可能かを表す状態 |
| 二重委譲防止 | main の `todo` と生存する `issue-N` session の照合 | 複数統括間の原子的 claim |
| worker 起動 | `session_delegate_issue`、launch/live prompt queue、TUI autostart | DAG 全体の concurrency 制御 |
| 生存観測 | `session_status`、agent phase、PR/merged 情報 | 終端理由と通知 event |
| 再開 | prompt queue の永続化、TUI 起動時の drain | orchestration decision の永続化 |
| バックグラウンド | TUI/daemon の session monitor | timeout/backoff を含む reconcile |

現状だけでも、統括 agent が main の ready issue を列挙し、worker を委譲し、`session_status` を定期確認して次を委譲する運用は可能である。prompt queue は統括不在時の連絡を保持し、TUI の autostart が再起動を助ける。ただし agent のプロンプト遵守に依存し、通知の重複排除、claim、retry、stack metadata は人手管理になる。

## 基本構造

```text
root または orchestrator session（DAG owner、1つ）
  ├─ issue-201 worker
  ├─ issue-202 worker
  └─ issue-203 worker

TUI sync/autostart tick
  └─ 永続 plan を reconcile → owner/worker の queued prompt を追加
```

plan は `orchestrator_id`、対象 issue 集合、owner session、同時実行上限を持つ。worker の `started_from` はすべて owner を指す。作業枠は `min(plan.max_parallel, agent 同時実行上限の空き)` とし、同じ issue に active claim があれば生成しない。

## 現状可否

| 要求 | 現状 | 判定 |
|---|---|---|
| 複数 issue の取得・DAG 判定 | `dependson`、issue graph/search/get、main 基準の ready | 手動 reconcile なら可能 |
| 子・孫 session への委譲 | `session_delegate_issue` と `started_from`。sandbox 条件により子から sibling 作成できない場合は root へ戻す | 可能だが、多段 tree は復旧と容量計算が難しいため非推奨 |
| PR 作成後の親報告 | worker が `session_prompt` で呼び出し元または `:root` へ報告でき、queue は永続化される | agent の遵守に依存するため部分的 |
| CLI・model 指定 | delegate/create/prompt の `agent_cli` / `model` と session record 上書き | 実装済み |
| queue・自動開始・同時数制限 | launch/live prompt queue、TUI autostart、`autostart_queued_prompt_limit` | 個別 session には実装済み。plan 単位の公平性・予約は未実装 |
| timeout・retry・CI/review 待ち | phase、session/PR/merged 観測は可能 | policy、attempt、deadline、CI/review adapter が未実装 |
| 親 agent 終了後の復旧 | queue と session record は残り、TUI が queued prompt を起動する | 判断状態・ack が無いため部分的 |
| merge 完了を依存 gate にする | main の issue `done` と `session_status.merged` / `session_pr` | 通常委譲では可能。stacked work-ready は未実装 |
| Claude/Codex 共通 skill | bundled skill は `.claude/skills` のみに symlink | skill 本文は共通化可能だが Codex discovery への配布が未実装 |

したがって、現行機能だけで提供できるのは「生存中の統括 agent が main-ready を観測して委譲し、worker の規約ベース報告を受けて再走査する」best-effort 運用である。全 issue 完了までの無人継続、exactly-once に近い委譲、失敗分類、親終了後の自動収束は durable coordinator の追加実装を要する。

## agent skill

### 責務境界

`usagi-orchestrate-issues` skill は coordinator/worker が同じ MCP 語彙と安全規約を使うための薄いクライアントにする。DAG state、claim、retry count、時刻、PR/CI 観測結果を会話や skill ファイルに保持せず、durable coordinator API の結果に従う。

```text
SKILL.md
  ├─ coordinator: plan 作成/再開 → reconcile → 提示 action の実行 → ack
  ├─ worker: issue 着手 → PR 作成 → terminal result の記録
  └─ safety: main merge と work-ready を混同しない、join/drift/conflict を escalate

usagi durable coordinator
  └─ state/event/claim/queue/dependency/timeout/retry の正本
```

skill は `name` と `description` だけの YAML frontmatter と、両 agent で通じる Markdown 命令を正本にする。Claude 固有の `context`、`agent`、`allowed-tools`、動的 shell 展開や Codex 固有 UI metadata は共通 `SKILL.md` に入れない。必要になった surface 固有 metadata は生成物または別ファイルに分離する。詳細 schema を skill 本文へ複製せず、必要なら同梱 `references/` に MCP workflow と状態の読み方だけを置く。

### 配置と注入

```text
assets/skills/usagi-orchestrate-issues/SKILL.md  # binary 埋め込み正本
                 │ materialize
                 v
<data-dir>/skills/usagi-orchestrate-issues/
                 ├─ symlink → <worktree>/.claude/skills/usagi-orchestrate-issues
                 └─ symlink → <worktree>/.agents/skills/usagi-orchestrate-issues
```

Claude Code は project skill を `.claude/skills`、Codex は repo skill を `.agents/skills` から発見し、どちらも symlink された directory と Agent Skills の基本形式を扱える。現行 `infrastructure::skills` は前者だけを作るため、後者と両 path の git exclude、衝突時に project 所有 directory を上書きしない規則を追加する。workspace root coordinator と全 session worktree の作成・復旧時に同じ link 処理を通し、選択した `agent_cli` にかかわらず skill が見えるようにする。

旧 `.claude/commands` は Claude で互換だが Codex と共有できず、supporting files も持ちにくいため新規経路には採用しない。初期段階の skill は既存 MCP だけを用いる best-effort mode と、durable coordinator tool が利用可能な mode を明示的に分け、存在しない tool を推測して呼ばない。

## 二種類の readiness

| 判定 | 意味 | 根拠 | 用途 |
|---|---|---|---|
| `work_ready` | 安全な基点があり実装を開始できる | 先行 worker の commit/PR head、claim、stack policy | worker 起動 |
| `merge_ready` | main に順序どおり merge できる | 既存どおり、全 `dependson` が main で `done` | PR merge |

issue の `ready` は `merge_ready` のまま維持する。`work_ready` は issue frontmatter に書かず、orchestrator plan の派生状態にする。これにより main 基準の CLI/TUI/root ready 判定を壊さない。

後続作業の基点は次の優先順で決める。

1. 全依存が main に merge 済みなら `main`。
2. 未 merge の依存が一本の祖先 chain なら、その先端 branch/commit。
3. 複数の未 merge 依存がある join node は、自動で仮 merge branch を作らない。依存 PR が main に揃うまで待つか、人が明示した integration base を使う。

## 永続状態

workspace ごとに、一つの stamped JSON envelope と append-only event queue を保存する。git 追跡対象にはしない。

```text
<workspace>/.usagi/orchestrators/<id>/state.json
<workspace>/.usagi/orchestrators/<id>/events/<event-id>.json
```

`state.json` は revision と lease を持ち、lock 下の compare-and-swap で更新する。主な内容は次のとおり。

| 単位 | 主なフィールド |
|---|---|
| plan | id、owner session、issue 集合、max parallel、revision、lease |
| node | issue、state、attempt、worker、base ref、PR、依存 PR、deadline、next retry |
| delivery | event id、target、queued/delivered/acked、attempt、next retry |

node state は `blocked`、`runnable`、`delegating`、`running`、`pr_open`、`review_wait`、`ci_wait`、`ci_failed`、`retry_wait`、`merge_wait`、`merged`、`failed`、`cancelled` を持つ。`delegating` を永続化してから session を作り、再起動後は session の存在を照合して `running` または `runnable` に収束させる。

## reconcile

reconcile は入力 snapshot と現在時刻から action を返す純粋な状態遷移にする。action の実行後に観測を更新し、次 tick で収束させる。

```text
load + lock/CAS
  → issue(main) / session / agent phase / PR / CI を観測
  → expired lease を回収
  → event を適用
  → timeout / retry / dependency / capacity を評価
  → delegate・prompt・escalate action を記録
  → action 実行
  → 次 tick で再観測
```

実行契機は TUI の既存 sync/autostart tick を第一候補とする。owner が `ended` または不在で runnable/actionable node があれば、owner 宛 launch queue に一つの集約 prompt を置く。TUI が閉じている間は状態と event が残り、次回起動時に再開する。既存 daemon は将来同じ reconcile entry point を呼べるが、専用 daemon は追加しない。

## 終端通知

worker の終了は agent の文面ではなく、session/agent ライフサイクル境界が event を発行する。最低限 `pr_opened`、`succeeded`、`failed`、`interrupted`、`timed_out` を区別する。

- event id は `orchestrator_id + issue + worker generation + terminal kind + terminal revision` から決定的に作る。
- event file の atomic create を重複排除点にする。同じ hook の再実行は同じ id になり no-op となる。
- worker 再作成時は generation を増やし、古い worker の遅延 event を現行 attempt に適用しない。
- owner が live なら live queue、非在なら launch queueへ送る。通知先が無くても event は ack まで削除しない。
- prompt の queue 成功は delivery であって ack ではない。owner reconcile が event revision を state に反映して ack する。
- queue は通知の wake-up に使い、event 本体の正本にはしない。launch queue の置換 semantics による通知欠落を避けるためである。

## stacked PR

後続 PR は GitHub 上の base を先行 branch にせず、リポジトリ規約どおり `main` を base に保つ。branch 自体は先行 head を基点にできるが、PR 本文と orchestrator state に次を明記する。

- `Depends-on: #<PR>` と対応 issue。
- review 順序と「先行 merge 前は merge 禁止」。
- base commit と依存 head commit。
- 先行 merge 後に main へ rebase し、差分から先行分が消えたことを確認する手順。

依存 PR の head が force-push で変わった場合、後続は `merge_wait` ではなく `rebase_required` 相当の人手確認へ送る。複数依存の join、競合解消、先行 PR の内容変更は自動 rebase しない。branch protection と `main` base 強制を回避しない。

## 失敗と待機

| 状態 | 自動処理 | 上限後 |
|---|---|---|
| delegate/launch の一時失敗 | 指数 backoff + jitter | owner を起動し escalation |
| worker timeout | phase/session を再観測し一度 interrupt | retry または escalation |
| review 待ち | PR 更新時刻まで待つ。agent 枠を消費しない | SLA 超過を通知 |
| CI failure | failure fingerprint を記録し修正 prompt/session へ再投入 | 同一 fingerprint 反復で停止 |
| merge conflict / base drift | 自動 merge しない | 人へ escalation |

retry policy は既定で attempt 3 回、指数 backoff、最大遅延を持つ。同じ CI failure fingerprint に対する修正は最大 2 回とし、新しい commit または failure fingerprint の変化がなければ再投入しない。retry は同じ issue claim と generation 系列を引き継ぎ、別統括からの二重委譲を防ぐ。

## 競合と安全性

- issue claim は `(workspace, issue)` で一意にし、lock/CAS と lease を使う。lease 切れだけでは即再委譲せず、session/PR を再観測してから回収する。
- `issue-N` session の存在、branch、PR head、claim が不一致なら自動修復せず `conflict` escalation にする。
- issue status の単一書き手は worker のまま。統括は tracked issue file を更新しない。
- event は at-least-once、state 遷移は冪等にする。通知の exactly-once 表示は要求しないが、同じ event の action は一度だけ適用する。
- owner 交代は lease と明示的 takeover で行い、二つの owner が同時に delegate しないようにする。
- PR 未 merge で後続を開始しても `merge_ready` にはせず、GitHub の merge gate を弱めない。

## 段階的導入

| 段階 | issue | 内容 | 主な受け入れ条件 |
|---|---|---|---|
| 現状運用 | 実装 issue 外 | 一つの統括 session が DAG を保持し、main-ready を通常委譲する。直列 chain のみ先行 head から後続 branch を開始し、PR 本文に依存を明記する。人が `session_status`/CI を確認する | PR 作成を依存完了と誤認せず、main merge を確認する |
| durable core | #183 | plan/state/event store、純粋 reconcile、claim/lease | crash point と競合 owner から二重委譲せず収束する |
| lifecycle event | #184 | worker generation、決定的 event、delivery/ack、owner wake-up | 重複・遅延・owner 不在でも event を失わない |
| scheduler | #185 | TUI sync reconcile、capacity、dispatch、timeout/wait | 上限到達時は queue に残り、再起動後に一度だけ進む |
| PR policy | #186 | stacked metadata、GitHub PR/CI/review 観測、retry/escalation | merge-ready を弱めず、join/drift/conflict を自動統合しない |
| 共通 skill | #187 | 共通 `SKILL.md` と Claude/Codex discovery path への配布 | 両 CLI で同じ workflow が発見され、durable state を skill に複製しない |
| 正本化 | Epic #182 完了時 | 実装済み事項を orchestration/settings/data/command 文書へ移し proposal を畳む | 正本に未実装事項を混ぜない |
| 将来拡張 | 別途起票 | GitHub webhook adapter、daemon からの同じ reconcile 呼び出し、可視化、policy の workspace 設定化、明示 integration branch | poll/tick 実測で必要性を確認してから追加する |

最小実装は永続 core → lifecycle event → TUI reconcile/worker dispatch → PR/CI policy の順に積む。共通 skill は #183 の API が固まった後に並行着手できる。前段 PR 未 mergeで後段を開発する場合も、各 PR は `main` base と依存表示を維持する。

## 検証計画

- 同じ snapshot を二回 reconcile して delegate action が一度しか出ない。
- 二つの統括が同じ issue を claim し、一方だけ成功する。
- `delegating` 永続化直後、session 作成直後、event 作成直後の各 crash から収束する。
- owner 不在で event が残り、owner 再作成/TUI 再起動後に一度だけ適用される。
- stale generation の終端 event と重複 event が現 attempt を完了させない。
- agent concurrency 上限到達時は runnable のまま待ち、枠解放後に開始する。
- timeout、retry 上限、同一 CI failure fingerprint 反復が escalation へ遷移する。
- 先行 PR 未 merge時の後続は `work_ready` でも `merge_ready` にならない。
- 先行 head drift、join node、競合で自動 rebase/mergeしない。
- main 上の issue `ready` の既存テスト結果が不変である。

## 設計判断

| 判断 | 採用理由 |
|---|---|
| issue ready を変更しない | main 基準の委譲・merge 判定との後方互換を守る |
| queue と event 正本を分離 | queue の one-shot/置換と ack を混同せず再起動に耐える |
| TUI tick を scheduler に再利用 | 新しい常駐 daemon を増やさず既存 autostart と統合できる |
| root/一統括が DAG owner | 多段委譲の観測漏れと agent 枠の再帰的消費を避ける |
| PR base は main | branch protection とレビューの基準を維持する |
| join/競合は人へ | 暗黙の仮 merge と force-push による誤統合を避ける |
