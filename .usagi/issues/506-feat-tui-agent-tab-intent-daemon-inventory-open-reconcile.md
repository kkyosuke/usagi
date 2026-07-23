---
number: 506
title: feat(tui): Agent tab intent を永続化し daemon inventory と再 open 時に reconcile する
status: done
priority: high
labels: [review, v2, tui, agent, persistence, recovery]
dependson: [509]
related: [388, 463, 503, 507, 508, 521, 526, 527]
parent: 505
created_at: 2026-07-21T21:20:48.446114+00:00
updated_at: 2026-07-23T00:09:07.139578+00:00
---

## 問題・影響

[PaneRegistry](../../crates/tui/src/usecase/application/pane.rs) は process memory のみで保持され、workspace open 時の [restore](../../crates/tui/src/presentation/mod.rs) は daemon の live inventory から tab を作り直す。そのため TUI を閉じると、利用者が開いていた Claude / Codex tab の順序・選択・明示的に閉じた状態を失う。再 open 時は inventory 順の tab が再出現し、閉じたが daemon 上では継続中の Agent も再表示される。

また restore は初回 frame 後に一度だけ同期実行され、inventory / attach の一時失敗を retry しない。復元した全 live tab を attach するため、「foreground tab だけ attach し、background は detached」という #388 の契約とも一致しない。

## 対象責務

data-dir の user-local / workspace-scoped store（`<data-dir>/tui/workspaces/<workspace-id>/agent-tabs.json`）に、secret-free な `AgentTabIntent` を versioned / atomic に保存する。`AgentTabIntent` domain / reconcile と persistence port は `usagi-tui` の domain/usecase 境界に置き、core domain へ UI intent を入れない。合成 root adapter が private directory/file、file lock、CAS、atomic publish、corrupt quarantine、future-schema read-only を束縛する。最低限、workspace identity、root または managed-session target、#509 が発行する durable `AgentContinuationRef`、last-known の完全な `TerminalRef`、tab 順序、選択、利用者が閉じた conversation lineage の dismissal を持つ。`AgentContinuationRef` は live inventory と resumable inventory に共通し、provider-native ID を含まない opaque key とする。

永続 state は表示 intent に限り、runtime liveness と PTY ownership の正本にはしない。open / daemon reconnect 時に unified inventory と照合し、次の規則で還元する。

| saved intent / inventory | 動作 |
|---|---|
| exact `TerminalRef` が current daemon inventory で live | 保存順で tab を復元し、保存選択を候補にする |
| inventory にだけ新しい live runtime がある | deterministic に末尾へ追加する。別 client が起動した Agent を欠落させない |
| dismissed lineage の durable history が live / interrupted / resume unavailable のいずれかで残る | tab を再表示しない。runtime / provider conversation は停止・削除しない |
| 利用者が dismissed lineage を明示 reopen した | 該当 dismissal だけを解除する。inventory 欠落、transport failure、snapshot の差異だけでは解除しない |
| saved terminal ref が non-live / 欠落だが同じ continuation が resumable | slot intent を保持して #510 へ引き渡す。interrupted tab への投影自体は #510 が所有する |
| saved terminal の generation が current daemon と異なる | #506 では attach しない。planned active / draining lifecycle と owner routing は #507 / #508 に委ねる |
| corrupt schema | private peer へ quarantine して空 intent から再構築する |
| future schema | 元 bytes を保持して read-only にし、restore / mutation を適用せず typed notice を表示する |

復元は UI event loop と別の専用 daemon connection / port で行い、初回 frame、キー入力、animation を待たせない。`terminal inventory → Agent inventory → terminal inventory` の前後 snapshot と live Agent の対応が coherent な全量 observation だけを適用し、partial / cross-RPC 不整合を generic-only restore として部分適用しない。完全一致する terminal row の重複だけを normalize し、同じ fenced ref の conflicting kind / live、duplicate live continuation、Agent↔terminal の非全単射は observation 全体を拒否する。一時的な transport failure は controller-level capped exponential backoff または typed daemon reconnect event で再試行し、同じ snapshot の replay は exact ref で 1 tab に収束させる。restore job は dispatch 時の UI interaction / registry revision を持ち、遅延結果を durable Observe より先に全拒否して dedicated port で fresh fence の observation を一度だけ即時再送する。transport failure と fence rejection が同時なら outage backoff / coalesced notice を優先する。

成功後も dedicated restore port を controller が保持する。restore socket の passive EOF を検知し、current endpoint が再び接続可能になった時だけ monotonic / coalesced connection epoch を発行して、epoch ごとに fresh observation を一度再送する。frame tick 自体は inventory RPC を発行しない。request の実効 deadline は #521、steady foreground polling scheduler は #527 の責務であり、#506 は restore request の off-thread 隔離だけを保証する。

attach / resync は現在表示中の active target に属する selected foreground tab だけに行い、各 background target の saved selection と選択外 tab は detached のまま保持する。保存済み selection が消失した場合は同じ target 内の次の surviving tab、なければ target の空状態へ縮退する。

成功した lifecycle snapshot の available session 集合は target 存在について authoritative とする。集合変更は controller の coalesced observation を1件要求し、outage backoff を短絡せず、旧集合で in-flight の結果は session-set fence で拒否する。集合から消えた session は target の selection / slots と、それら slots が所有する dismissal だけを同一 commit で除去する。他 target / dismissal は保持する。session が allowed のまま inventory から runtime だけが欠落した場合は dormant target / dismissal を保持し、history retention / GC の根拠にしない。

order / selection / close の確定 mutation ごとに state を atomic commit する。複数 TUI process が同じ workspace state を更新する場合は file lock と revision / compare-and-swap で read-modify-write を直列化し、CAS conflict は最新 state を再読込して stable key ごとに merge する。stale Observe は最新 exact ref を stale candidate に置換せず fresh observation を要求し、stale Reopen / admission は新しい Dismiss を解除しない。dismissal は明示 reopen まで union し、遅い writer が新しい close intent を失わせない。保存失敗時は close / reorder / selection / reopen の可視 UI を変えず typed safe notice を表示する。coherent restore の Observe 保存が失敗した場合も既存 pane / order / selection を維持してinventory-only Agentを表示せず、新しい generic ref だけを append / exact-dedup する。inventory 失敗時は直前の valid state を空 snapshot で上書きしない。

利用者が dismissed lineage を戻せる `Reopen closed Agent` 操作を提供し、safe label と `AgentContinuationRef` だけで対象を選ぶ。reopen は dismissal を atomic に解除した後、dedicated port の fresh coherent observation から既存 live / interrupted slot を再表示する。過去の inventory cache で pane 一覧を置換せず、Agent spawn / provider resume は発火しない。

## 非対象

- provider conversation の自動 resume、Agent の local spawn、runtime の kill。
- Agent history / exit history / dismissal の retention・allocator・GC policy（#526）。
- multi-generation endpoint routing（#507 / #508）、`ClientPolicy` / Unix stream timeout（#521）、`InputAck` reconnect replay、steady terminal poll scheduler（#527）。
- generic Terminal / document tab の本文永続化。これらは schema 上で安全に無視・移行でき、既存挙動を壊さないことだけを保証する。
- pending tab の blind replay。初期実装で cancel できるのは daemon へ未送信の client-owned queued launch だけとする。送信済み / in-flight operation は再送・推測 cancel せず、reopen 後に完成済み inventory / durable outcome へ収束させる。将来 pending を永続化する場合は TUI と daemon が同じ `OperationId` を共有し、outcome query / replay で二重 launch を防ぐことを前提とする。
- repository へ UI state を commit すること。保存先は data dir の user-local / workspace-scoped resume state とし、provider ID、transcript、terminal output、argv、environment 値を保存しない。

## 受入条件

- [x] Claude / Codex の複数 live tab を持つ TUI を正常終了して fresh TUI で 2 回 reopen すると、exact `TerminalRef`・順序・非先頭 selection が復元され、Agent PID と spawn count は変わらない。
- [x] shipping TUI process を `SIGKILL` して abrupt EOF にした後も daemon PID / generation は不変で、fresh shipping TUI が同じ Agent / generic Terminal の exact `TerminalRef` と child PID に再 attach し、spawn count を増やさず retained output を replay して新しい input echo を返す。その TUI を正常終了した後の second fresh reopen でも同じ identity / replay / input を維持する。
- [x] tab close は subscription だけを detach して continuation-scoped dismissal を durable にし、同じ lineage の interrupted / replacement incarnation は明示 reopen まで再出現しない。別の新しい conversation lineage は表示される。
- [x] `Reopen closed Agent` は選んだ lineage の dismissal だけを解除し、既存 tab を一度だけ再表示する。provider resume / replacement spawn を暗黙に行わない。
- [x] inventory-only の新 runtime、保存済み runtime、exact-equal duplicate snapshot が deterministic に 1 枚ずつへ収束し、conflicting duplicate / duplicate live continuation / 非全単射は全体を retry する。
- [x] 初回 frame / key input は遅い inventory でブロックされず、選択 tab だけが attach / resync される。
- [x] restore dispatch 後の close / reorder / selection で遅延 response の fence が外れた場合、runtime と on-disk bytes / revision を変更せず、専用 port で fresh fence の observation を exactly once 再送する。background target の selection は attach や focus change を発生させない。
- [x] daemon 一時不通後、success → passive EOF → endpoint available の connection epoch ごとに restore を exactly once 再試行し、誤 spawn / focus steal を起こさず安全な feedback を表示する。通常 frame は再観測を発行しない。
- [x] persistent outage は single-flight / capped controller backoff / notice coalescing で worker churn を抑え、transport failure と stale fence が同時でも key activity が backoff を迂回しない。
- [x] partial / cross-RPC 不整合 inventory は Agent / generic pane のどちらも部分適用せず、coherent な fresh observation まで retry する。
- [x] stale target、scope / generation mismatch、corrupt / old / future state、concurrent client の lock / revision conflict で誤 attach・lost update・二重 tab・起動失敗を起こさない。durable replacement `R` と stale observation `O` が競合した projection に `O` を出さず、fresh observation だけが `R` を復元する。stale Reopen と newer Dismiss の競合は close intent が勝つ。
- [x] disk / permission / future-schema などの durable mutation failure は typed notice になり、bytes と close / reorder / selection / reopen の可視 UI を成功扱いで変更しない。
- [x] authoritative session removal は available 集合変更から coalesced observation を exactly once 発行し、removed target の selected / dismissed slots とその dismissal を同時に除去し、他 target を復元して unrelated dismissal を失わない。allowed session 内の inventory 欠落は dormant state を保持する。
- [x] generic Terminal tab は従来どおり unified inventory から復元され、Agent intent / dismissal / same-TUI Reopen によって欠落・抑止・重複しない。Agent close 後の generic successor と generic selection を維持し、mixed inventory の intent 保存失敗でも既存 pane を変えず新 genericだけを一度追加する。

## 必須回帰テスト

- shipping TUI process、実 daemon socket / PTY、Claude / Codex fixture を使い、launch → unique output → normal quit → fresh TUI open → retained output → input echo を検証する。次に TUI process を `SIGKILL` して abrupt EOF を発生させ、fresh shipping TUI が同じ daemon PID / generation、exact `TerminalRef`、Agent child PID、spawn count のまま retained output / input echo を継続し、normal quit 後の second fresh reopen でも同じ identity / replay / input を維持することを固定する。planned daemon restart test と混同しない。
- mixed Claude / Codex、root / session、tab reorder / selection / close、新 runtime append を同じ product E2E に含め、起動した lineage ごとの child PID と exact spawn count（Codex 1、root/session Claude 合計 2）が不変であることを固定する。
- generic Terminal fixture でも normal quit と TUI `SIGKILL` の両方から fresh shipping TUI を開き、same daemon PID / generation・same exact `TerminalRef`・same child PID / spawn count・retained output replay・input echo を固定する。
- slow / failed / partial / cross-RPC 不整合 / out-of-order inventory、interaction / registry / durable revision fence、controller backoff、reconnect retry、duplicate / stale ref、corrupt / future schema、atomic write interruption、concurrent CAS / causal Dismiss-vs-Reopen merge、durable error UI rollback を deterministic fixture で検証する。

## docs / migration

[TUI](../../document/03-tui.md) と [workspace pane restore proposal](../../document/proposals/11-workspace-restore-panes.md) の「pane 一覧を永続化しない」を、daemon inventory が liveness の正本、local state が表示 intent の正本という two-source reconciliation 契約へ更新する。旧 state 欠落は空 intent として互換に読み、推測 migration を行わない。
