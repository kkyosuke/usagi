---
number: 774
title: "fix(daemon): 応答しない daemon が「running」と報告され、restart で復旧できない"
status: done
priority: high
labels: [v2, daemon, lifecycle, diagnostics, correctness]
dependson: []
related: [773, 771]
created_at: 2026-09-28T00:00:00+00:00
updated_at: 2026-09-28T00:00:00+00:00
---

## 問題

daemon の background worker が macOS の condvar から `EINVAL` を受けて panic し（`assertion failed:
r == libc::ETIMEDOUT || r == 0`、std の `pthread_cond_timedwait_relative_np` 直後の assert）、process は
生きたまま serve を止めた。その後、利用者が使える復旧手段が 1 つも無かった。

```text
$ usagi update
daemon synchronization refused [unavailable]: daemon transport is unavailable
Error: installed usagi could not complete daemon synchronization; inspect 'usagi daemon status' before retrying

$ usagi daemon status
usagi v4.8.5: daemon running (pid 68818)        # 応答しないのに running

$ usagi daemon restart --restart-agents
usagierror: Unavailable: Connection refused (os error 61)
```

TUI も `daemon unavailable: Lifecycle: daemon did not become ready` だけを出す。

## 根拠

lifecycle の観測はすべて **process 生存**（recorded pid が exact process-start identity を保つか）で、
**endpoint が応答するか**を誰も見ていない。そのため「process は生きているが serve していない」状態が
どの入口からも「健全な daemon」に見える。

| 入口 | 起きたこと | 原因 |
|---|---|---|
| `usagi daemon status` | `daemon running (pid N)` | `usecase::status::report` は record + liveness probe だけを見る |
| `usagi daemon restart` / `--restart-agents` | `Unavailable: Connection refused (os error 61)` | `owned_runtime` が Alive + live runtime と観測 → `plan_replacement` が `SeamlessRollover` を選ぶ → rollover は旧 active への IPC が前提で、その接続が拒否される。planned path には fallback が無く、`--force` は message のどこにも出ない |
| `usagi update` | `daemon transport is unavailable` | `managed_update_diagnostic_client` は `daemon owner is active but its endpoint is not ready` を返しているが、`write_client_error` の `ClientError::Unavailable(_)` arm が payload を捨てて固定文に置き換える |

panic 自体の特定も妨げられている。`format_panic` は panic した thread 名を記録せず、release profile が
`strip = true` のため backtrace は `__mh_execute_header` が 12 行並ぶだけになる。daemon の error log は
「transient か product 失敗か」を判定できる唯一の証拠（[06-conventions](../../document/06-conventions.md#daemon-e2e-の-transient-と-product-失敗を混同しない)）なのに、
どの worker が落ちたのかを誰も言えない。

## 方針

「recorded owner が endpoint に応答するか」を 1 つの観測として型にし、それを見る必要がある入口だけが払う。

| 変更 | 内容 |
|---|---|
| `EndpointObservation` | `NotObserved` / `Answering` / `Silent`。probe を払わない command は `NotObserved` のまま |
| `status` | Alive かつ `Silent` なら `daemon running but not answering (pid N)` と、`--force` の remedy を報告する |
| `seamless_refusal` | Alive だが `Silent` の active には `SeamlessRefusal::ActiveUnreachable` を返す。planned replacement は raw な transport error ではなく、`--force` を名指す refusal で止まる |
| `ClientError` | managed update が自分で書いた説明は `Unavailable` ではなく `Lifecycle` で返す。両者は `retry_mode` / `side_effect` / `code` / `is_transport_failure` が同一で、違うのは message が利用者に届くかどうかだけ |
| panic 診断 | `format_panic` が thread 名を記録し、release profile を `strip = "debuginfo"` にして backtrace が frame 名を持つ |

probe は **planned な** replacement と status だけが払う。cold な replacement（`--force`）は recorded owner に
何も尋ねずに signal するため、refusal を読まない経路に probe 代を払わせない。`serve` は自分が publish する前の
endpoint へ接続しにいくことになるため絶対に probe しない。

probe の上限は **attempt 数**で置く。1 attempt は `TerminalLaneBudget::CONNECT_MS` を自分で持つので、
wall clock の上限にすると「socket が無くて即失敗する」場合と「accept はされるが hello が止まる」場合とで
attempt 数が桁で変わり、後者では 2 回しか試さない。accept backlog の一時的な混雑を「沈黙」と読み違えないことが
この観測の唯一守るべき性質なので、試行回数は見えている数でなければならない。

live runtime を持つ unreachable な daemon を自動で cold 置換はしない。到達できない daemon が PTY を
まだ所有している可能性は残り、planned transition の契約（live runtime を壊さない）を破るため。利用者へは
壊す選択肢（`--force`）を名指しで示す。

## 受入条件

- Alive + `Silent` の record で `status` が「応答していない」と報告し、remedy を名指す。✅ `usecase::status` の unit test
- Alive + `Silent` + live runtime の planned replacement が `ActiveUnreachable` で refuse し、rollover IPC に到達しない。
  ✅ `usecase::replacement` の unit test
- live runtime が無ければ従来どおり cold transition で restart できる（probe の有無で変わらない）。✅ 同 unit test
- `Answering` / `NotObserved` は現行の判定を一切変えない。✅ 同 unit test
- managed update の「endpoint が ready でない」説明が stderr に出る。
  ✅ `runtime::cli::tests::a_lifecycle_failure_reaches_the_terminal_with_its_own_words`
- panic log が thread 名を含む。✅ `runtime::daemon::tests::a_recorded_panic_names_the_thread_it_happened_on`
  （`format_panic` から抽出した `panic_report` を検証する。`format_panic` 自身は実 hook の束ね）
- 拒否は、その拒否を実際に解く経路だけを提示する。`--restart-agents` は planned のまま同じ endpoint を
  必要とするので `ActiveUnreachable` では名指さない。
  ✅ `usecase::replacement::tests::every_refusal_offers_only_the_path_that_clears_it`
- probe の上限は attempt 数で、1 attempt あたりの budget に左右されない。
  ✅ `runtime::daemon::tests::an_endpoint_probe_asks_every_attempt_before_reporting_silence`
