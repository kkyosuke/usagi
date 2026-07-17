# 提案: PTY crash 継続（broker／FD handoff）

> [設計提案の目次](README.md) ｜ [ドキュメント目次](../README.md) ｜ ← 前へ [TUI v1 parity](06-tui-v1-parity.md) ｜ 次へ → [agent dispatch MCP](08-agent-dispatch-mcp.md)

本書は **v2 MVP の後にだけ評価する未実装設計**である。PTY master fd を daemon crash 後にも継続利用するための broker と planned restart 時の Unix FD handoff を比較し、採否と着手条件を定める。本書がこの将来機構の設計判断の正本であり、MVP の crash 契約は [5. daemon](../05-daemon.md#generation-と-orphan-safety) を正本とする。

## 目次

- [前提と非目標](#前提と非目標)
- [方式比較と採否](#方式比較と採否)
- [failure matrix](#failure-matrix)
- [threat model](#threat-model)
- [採用時の ownership と protocol](#採用時の-ownership-と-protocol)
- [実装前提と issue 分割](#実装前提と-issue-分割)

## 前提と非目標

MVP の daemon は PTY master fd、VT100 状態、出力 journal を所有する。daemon が crash すると child process が生存していてもこれらを復元できないため、`orphan_running` または `identity_unknown` として attach、input、暗黙の replacement を拒否する。この explicit orphan 契約を変更するのは、本書で採用する機構を実装した場合だけである。

PID、process group、`kill(0)`、端末 slave path、再生成した socket だけから master fd や画面を adopt する方式は採らない。FD を持たない新 daemon が「継続した」と表示することも禁止する。Windows は Unix FD handoff の対象外であり、同等性を仮定しない。

## 方式比較と採否

| 方式 | crash 後の live attach | 追加する常駐境界 | planned rollover | 主な故障領域 | 判断 |
|---|---|---|---|---|---|
| explicit orphan（MVP） | 不可。安全に orphan を表示 | なし | old daemon を draining | daemon crash と child の分離 | 採用（MVP） |
| Unix `SCM_RIGHTS` handoff | 不可。handoff 前の daemon crash では失う | なし | old daemon から new daemon へ master fd を移送 | transfer 中断、old/new split brain、FD close 漏れ | broker より先には採用しない |
| 外部 PTY broker | 可。broker が生存し journal が保持される場合 | broker と supervisor | daemon は control を rollover、broker は terminal を継続 | broker crash、認可、二重 attach、journal 枯渇 | 条件付きで将来採用候補 |

採否は「MVP には不採用、複数の daemon crash 後にも interactive session を継続することが製品要件になった時だけ broker を検討する」とする。`SCM_RIGHTS` 単独は planned restart の待ち時間や generation 数を減らせるが、crash 継続という目的を満たさず、old daemon を trusted FD sender として残す複雑性を増やす。broker を採用した後でも handoff は broker の planned upgrade に限定した最適化として別途評価する。

broker を導入しても daemon の control authority と durable session/operation state は daemon に残す。broker は terminal data plane のみを所有し、daemon crash を broker crash と同一障害にしない。

## failure matrix

| 失敗点 | explicit orphan（MVP） | FD handoff | broker | fail-closed の結果 |
|---|---|---|---|---|
| daemon が通常 crash | orphan として reconcile | handoff 未完了なら orphan | broker session を再発見して再 attach | ownership を推測しない |
| old daemon が FD send 前に crash | orphan | orphan | 該当なし | input を再送しない |
| FD send 後、receiver ACK 前に crash | 該当なし | 両者を terminal owner と認めず recovery record で判定 | 該当なし | terminal を orphan 扱いにする |
| broker が crash | orphan | 該当なし | child の identity を確認して orphan/lost | broker 再起動で自動 adopt しない |
| broker journal が破損・cursor を失う | 該当なし | 該当なし | snapshot/resync を拒否し terminal を attach-blocked | 古い画面を捏造しない |
| broker と daemon の接続断 | client は daemon 状態を維持 | handoff を abort | terminal は `broker_unreachable` | input/kill を blind retry しない |
| daemon generation が stale | request を拒否 | receiver を拒否 | broker lease を拒否 | stale generation は write 不可 |
| client が遅い／切断 | detach のみ | 同左 | broker は subscriber を落とし cursor/resync | PTY drain を止めない |
| broker upgrade 中に失敗 | 該当なし | 該当なし | old broker を owner のまま残す | atomic ownership commit 前に切替えない |

FD handoff の receiver ACK は「FD を受け取った」だけでは不十分である。新 owner が terminal identity、protocol generation、journal cursor を durable に記録し、old owner が write を停止したことまで同じ handoff transaction で確認できない場合、双方を write-capable にしない。

## threat model

### 保護対象と信頼境界

保護対象は terminal input/output、PTY master fd、child process group、scrollback、terminal capability と、terminal を操作できる lease である。broker は daemon より小さくしても、master fd を持つため daemon と同等以上の機密性・完全性境界になる。Unix domain socket の pathname だけを認証根拠にしない。

| 脅威 | 攻撃または事故 | 必須の軽減策 |
|---|---|---|
| FD 奪取 | 別 UID/process が broker socket へ connect し FD を要求 | runtime directory の owner/mode、peer credential 検証、broker 発行 capability、FD を client へ渡さない |
| stale daemon write | crash 前の daemon が復帰・遅延し input/kill を送る | broker lease に `BrokerEpoch` と `DaemonGeneration` を bind し、各 mutation で fence を検証 |
| confused deputy | session A の capability で session B の terminal を操作 | capability に workspace/session/terminal incarnation と権限を含め、broker 側で全 scope を再検証 |
| replay / duplicate input | reconnect や ACK loss で bytes を二重送信 | durable `OperationId` / `RequestId` と input result cache。partial write は ambiguous |
| resource exhaustion | client や terminal が journal、FD、subscriber を使い尽くす | terminal/subscriber/bytes の上限、bounded journal、per-peer rate limit、明示的 resync |
| malicious or incompatible upgrade | 新 daemon/broker が異なる wire/state を解釈する | protocol generation、capability negotiation、upgrade 前 compatibility gate、downgrade 拒否 |
| broker compromise/crash | master fd と出力が漏洩・喪失する | 最小 privilege、supervisor、crash audit、orphan fallback。broker 自動復旧後の adopt 禁止 |

broker は input の認可・fencing を daemon のみへ委譲してはならない。daemon が crash または network partition した時に stale daemon を区別できないため、broker が lease と scope をローカルに検証する必要がある。一方、raw launch command、環境変数、任意ファイル path は broker protocol に加えない。

## 採用時の ownership と protocol

### ownership

```text
client ── authenticated IPC ──► daemon (control authority)
                                      │ terminal command + fenced lease
                                      ▼
                               broker (PTY master / vt100 / journal)
                                      │
                                      ▼
                               child process group
```

| 資源 | MVP owner | broker 採用後の owner | daemon crash 時 |
|---|---|---|---|
| PTY master / child watch | daemon generation | `BrokerTerminal` | broker が保持 |
| VT100 state / raw journal / cursor | daemon generation | `BrokerTerminal` | broker が保持 |
| terminal identity / incarnation | daemon registry | durable registry と broker の照合 | daemon が再照合 |
| session/control / operation journal | daemon generation | daemon generation | daemon recovery が継続 |
| input/kill authorization | daemon generation | daemon 発行 lease を broker が検証 | stale lease を拒否 |

`TerminalRef` は維持し、`broker_terminal_id` と `broker_epoch` を導入する。`DaemonGeneration` は terminal の物理 owner ではなく control lease の発行者になる。broker は `TerminalRef` の scope と一致しない command、期限切れ lease、異なる protocol generation を拒否する。

### protocol 境界

daemon-broker protocol には `BrokerHello`、`TerminalCreate`、`TerminalAttach`、`TerminalInput`、`TerminalResize`、`TerminalKill`、`TerminalSnapshot`、`TerminalOutput`、`TerminalExit`、`LeaseGrant`、`LeaseRevoke`、`BrokerReconcile` を置く。すべてに `BrokerEpoch`、`TerminalRef`、protocol generation、相関 ID を含める。input/kill は既存の idempotency / ambiguity 契約を保つ。

broker は client-facing IPC を公開しない。client は daemon の IPC だけを使い、daemon は crash 後に認証済みの broker endpoint を registry から再解決して `BrokerReconcile`、snapshot、cursor resume を行う。broker が再起動した、または epoch/journal の連続性を証明できない場合は成功した attach とせず MVP と同じ orphan/lost 経路へ落とす。

Unix `SCM_RIGHTS` を採る場合も、FD と共に terminal identity、sender/receiver epoch、protocol generation、journal cursor、one-time transfer nonce を送る。受信側の durable accept と旧所有者の write revoke が確認されるまで client mutation を止める。Windows はこの transport を実装せず、broker 方式も Windows の handle duplication・security model を個別に設計してから判断する。

## 実装前提と issue 分割

着手は #209/#220 の MVP cutover が production で安定し、orphan/reconcile、bounded output、generation fencing、端末 API の実測値が得られた後に限る。次のいずれかを満たさなければ本設計を実装しない。

- daemon crash 後の interactive session 継続が明示的な製品要件になった。
- broker 常駐による memory、FD、監視、security review のコストが、orphan による作業損失を上回ると計測で示された。
- Unix 限定の提供範囲、broker supervisor、upgrade/rollback 運用、Windows の非対応または代替方針が承認された。

| 順序 | 独立 issue | 成果物 |
|---|---|---|
| 1 | `design(daemon): broker security and lifecycle contract` | lease/capability、epoch、crash/upgrade matrix、Unix/Windows support decision |
| 2 | `feat(core): broker protocol and fenced terminal ownership` | wire type、compatibility、pure reducer、idempotency test |
| 3 | `feat(daemon): supervised Unix PTY broker` | broker process、credential check、bounded journal、real PTY crash E2E |
| 4 | `feat(daemon): daemon recovery through broker reconcile` | restart attach/resume、orphan fallback、TUI/CLI/MCP integration |
| 5 | `experiment(daemon): SCM_RIGHTS broker upgrade handoff` | broker upgrade の FD transfer、fault injection、採否 |
| 6 | `design(platform): Windows terminal continuity` | handle/ConPTY/security model を別途評価。Unix 設計の移植を前提にしない |

各 issue は broker crash、daemon crash、partition、stale generation、ACK loss、journal eviction、upgrade rollback を fake と real PTY E2E の両方で検証する。いずれかで continuity を証明できない terminal は、明示的に orphan/lost と表示する。
