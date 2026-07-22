---
number: 521
title: fix(ipc): ClientPolicyのrequest deadlineとreconnect budgetを実効化する
status: todo
priority: high
labels: [review, v2, core, ipc, client, resilience, timeout]
dependson: []
related: [197, 215, 216, 463, 489, 518, 519]
created_at: 2026-07-22T11:40:05.764143+00:00
updated_at: 2026-07-22T12:10:14.415776+00:00
---

## 問題・影響

`ClientPolicy` はsurface別の `timeout_ms` と `reconnect_attempts` を公開するが、shipping `IpcClient::request` はtimeoutをrequest envelopeへ載せるだけでblocking `read_json_frame` を無期限に続け、`reconnect_attempts` を一度も消費しない。server側もrequest timeoutをclientのwall-clock deadlineとして強制しない。

daemonがframe途中、request処理、response送信で停止するとTUIの同期requestがevent loopを止め、描画・キー入力・quitまで無期限blockする。CLI/MCPもsurface policyの時間内に戻らない。

## 既存issueとの境界

#215はtimeout/response-loss/retry identityのprotocol契約、#216はhandshake timeout/nonblocking transportを完了条件に含むが、現shipping client policyは実効化されていないためcorrective issueとする。terminal inputのcross-connection eligibilityは#519、generic Terminal Launchのdurable outcome/allocatorは#518が所有する。pane worker ownershipとTUI polling schedulerは別issue。

## retry eligibility table

| request class | same connection retry | new connection retry | required identity/evidence |
|---|---|---|---|
| read-only query | deadline内で可 | policy budget内で可 | full resource/generation fence。stale responseは捨てる |
| mutation with server-backed durable outcome | same operationのquery/replayのみ可 | policy budget内で可 | producer `OperationId` + semantic digestをserver durable storeが照合し、same operation finalを返す |
| mutation with only `RequestId` correlation | response loss後は不可 | **不可** | `RequestId` はconnection-local correlationに過ぎず、cross-connection idempotency evidenceではない |
| terminal input | #519完了までは不可 | #519完了までは不可 | #519のclient incarnation + stable input operation + digest + ordered ledger |
| generic Terminal Launch | #518完了までは不可 | #518完了までは不可 | #518のproducer OperationId + digest + durable launch outcome |

Agent等のmutationも、実際にserver-backed durable OperationId/digest contractを持つ経路だけがeligibleである。「同じRequestIdを再利用できる」はretry許可条件にしない。ineligible mutationはeffect unknownを返し、blind retryしない。

## deadline / attempt budget

- initial attemptと各reconnect attemptはそれぞれconnect/handshake/frame write/response readを含む**1つのend-to-end monotonic deadline budget**を消費する。
- partial progress、unrelated event、frame header/bodyの一部到着でattempt deadlineをresetしない。
- `reconnect_attempts=N` はinitial後の追加attemptを高々N回許す。最大wall-clockはattempt数×surface deadline + 明示されたbounded backoff/scheduler誤差として計測可能にする。
- eligibilityがないmutationはresponse loss時点で終了し、未使用reconnect budgetがあっても次attemptを開始しない。

## 対象責務

- connect/handshake、frame write、response待ちを含むattempt単位end-to-end monotonic deadlineをsurface policyから実効化する。
- blocking IOにOS read/write timeoutまたはdeadline-aware transportを適用し、partial frameやunrelated eventがdeadlineを延長しないようにする。
- `reconnect_attempts` を明示したretry state machineへ配線し、上表を唯一のeligibility判定にする。
- durable mutation retryでは同じproducer OperationId + semantic digestでoutcome query/replayし、新しいeffect requestを生成しない。
- budget exhaustedはtyped unavailable/timeoutで返し、side-effect state unknownを失敗確定としない。
- TUI quit/control laneがhung data requestの完了を待たず有界に進める構成へ接続する。

## 受入条件

- [ ] peerがhello前、request read後、partial response後、unrelated event連続中に停止しても各attemptがpolicy deadline+小さなscheduler誤差以内に戻る。
- [ ] reconnect回数はpolicyどおりで0/1/N、success after eligible retry、budget exhaustionを観測でき、各attemptが独立のend-to-end budgetを1つ消費する。
- [ ] read-onlyだけはnew connectionで安全にretryできる。
- [ ] durable mutationはserver-backed OperationId + semantic digest一致時だけsame finalへ収束する。
- [ ] RequestIdしかないmutation、#519前terminal input、#518前generic launchは未使用budgetがあってもnew connectionへblind retryしない。
- [ ] timeout後もTUI draw/input/quit、CLI exit、MCP response loopが無期限停止しない。
- [ ] late responseは新requestへ誤相関せず、partial frame/socketは再利用しない。

## 必須回帰テスト

- fake clock/deadline transportとUnixStream pairでhello stall、write stall、no response、partial header/body、wrong-request event flood、eligible retry success/exhaustionを検証する。
- read-onlyとdurable OperationId+digest mutationだけがretryされ、RequestId-only mutationのconnection count/attempt countが1であることをassertする。
- ineligible terminal input/generic launchをserverが一度PTY/spawnへ適用した後、ACKをpartial-writeして切断するresponse-loss fixtureで、PTY write/spawn effect countが**exactly 1**、client retry count 0、outcome effect unknownをassertする。
- request frame自体のpartial write/stallでserver dispatch前に切れたfixtureはeffect count 0、retry count 0をassertし、0か1かをclientが推測しないことを確認する。
- #519/#518 fixtureが利用可能になった後は同じoperation final queryへ収束し、effect count 1のまま追加attempt budgetを消費することを別testで固定する。
- TUI process testでhung daemon中のquit wall-clock boundを固定する。

## docs

`document/04-ipc.md` にsurface別attempt deadline、上記retry eligibility table、RequestIdのconnection-local性、unknown-effect outcome、budget exhaustionを記載する。
