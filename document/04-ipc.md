# 4. daemon IPC

> [ドキュメント目次](README.md) ｜ ← 前へ [3. TUI](03-tui.md) ｜ 次へ → [5. daemon](05-daemon.md)

daemon と各 client 面が共有する IPC の現在の契約である。クレート境界と実装の置き場所は
[2. アーキテクチャ](02-architecture.md) を正本とする。

## 目次

- [identity と fence](#identity-と-fence)
- [frame と handshake](#frame-と-handshake)
- [envelope とエラー](#envelope-とエラー)
- [Unix transport](#unix-transport)
- [client の失敗処理](#client-の失敗処理)
- [managed session request](#managed-session-request)
- [daemon metrics subscription](#daemon-metrics-subscription)
- [PR inventory snapshot](#pr-inventory-snapshot)
- [agent launch request](#agent-launch-request)
- [Codex structured capture request](#codex-structured-capture-request)
- [dispatch request](#dispatch-request)
- [generic terminal request](#generic-terminal-request)

## identity と fence

v2 の resource identity は lowercase canonical UUID の newtype である。表示名、path、PID、
daemon 内 counter は属性であり、effect を行う resource key ではない。`WorkspaceId`、`SessionId`、
`WorktreeId`、`TerminalId`、`AgentRuntimeId`、`AgentResumeSourceId`、`DaemonGeneration` は resource
incarnation ごとに新規発行される。`OperationId` は UUIDv7 の durable intent identity である。
`AgentContinuationRef` は provider conversation lineage ごとの daemon-issued public identity であり、
live runtime、中断した resume source、resume 後の replacement runtime に共通する。provider-native ID
とは別の opaque UUID であり、新しい conversation lineage へ再利用しない。

effecting terminal command は完全な `TerminalRef` を使う。これは daemon generation、terminal、
workspace、optional session、worktree の全 ID を含む。一つでも registry の entry と異なれば
`stale_target` であり、名前・path・単独 terminal ID による再探索はしない。Agent runtime も
`AgentRuntimeRef` で terminal と session に束縛する。

late worker completion は workspace、session、operation、owner generation、execution attempt、
lifecycle attempt、expected revision を含む `CompletionFence` を照合してから適用する。不一致の
completion は state mutation にしない。legacy state は typed incarnation を持たないため、managed
session state へ推測移行しない。

## frame と handshake

transport は u32 big-endian length prefix と JSON payload の frame を運ぶ。空 frame、negotiated
上限を超える frame、途中まで読んだ prefix の EOF はエラーである。prefix の前に EOF となった
場合だけ clean close とする。既定 frame 上限は 1 MiB であり、reader は長さを検証してから
payload を確保する。

`ClientPolicy.timeout_ms` / `reconnect_attempts` は surface policy vocabulary だが、現行 shipping UnixStream request は
connect / handshake / write / response read を一つの実効 monotonic deadline として強制していない。TUI の pane restore は
request を off-thread に隔離して frame / input / quit の同期待ちを避けるが、transport 自体の deadline と retry eligibility は
[#521](../.usagi/issues/521-fix-ipc-clientpolicy-request-deadline-reconnect-budget.md) が所有する。

最初の frame は必ず `ClientHello` である。hello は client ID、connection nonce、期待する
daemon generation、対応 protocol range、capability、build diagnostics を含む。daemon は generation /
revision の共通範囲と必須 capability を検証し、成功時に `ServerHello` を返す。build identity は wire
protocol の互換性判定には使わないが、client bootstrap は `ServerHello` の identity で同一 channel の
daemon が現在 binary と同じ build tuple かを確認する。現行 shipping client / server はどちらも
`{ version: CARGO_PKG_VERSION, commit: "unknown", target: ARCH }` を送り、bootstrap は version / target が非空なら
tuple の完全一致を same build とする。したがって同じ version / target の別 artifact は old daemon と同一と誤認され、
build replacement を発火しない。production composition は全 runtime mode で `force_restart = false` を渡すため、#275 が
定義した development 毎 bootstrap restart も未配線である。canonical artifact identity、全 channel の exact-artifact
reuse、明示 force replacement、unknown 時の fail-safe policy は
[#528](../.usagi/issues/528-fix-daemon-build-artifact-identity-safe-rollover-trigger.md) で追跡する。

mismatch を検出できた場合の現行 shipping lifecycle は旧 daemon を stop して fresh daemon を start する cold
replacement であり、旧 PTY を保持する active / draining rollover ではない。通常 envelope は handshake の成功後だけ
受理する。

## envelope とエラー

通常通信は protocol version と daemon generation を必ず持つ envelope である。

| kind | 相関子 | 用途 |
|---|---|---|
| request | `RequestId` | client の一回の RPC |
| response | 同じ `RequestId` | immediate result、accepted operation、または typed error |
| event | `SubscriptionId`、`StreamRef`、sequence | server push |

現行 production handler で `RequestId` は一回の RPC correlation にだけ使い、`ResponseCache` は接続されていない。
再接続すると client の request sequence も変わるため、`RequestId` を durable idempotency key として扱わない。
session / Agent / dispatch 等の durable mutation は request correlation と独立した `OperationId` を持ち、target
scope と semantic digest が同じ場合だけ既存 operation として再利用する。generic Terminal Launch はこの契約の
例外で、producer `OperationId` と durable replay をまだ持たない。この gap は
[#518](../.usagi/issues/518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) で追跡する。

`ProtocolError` は machine-readable な code、safe message、retry mode、side-effect classification、
error ID を返す。resource/ownership を証明できない場合は `ownership_unknown`、resume が成立しない
場合は `resync_required` を使う。OS error、secret、raw launch provision は error detail に含めない。

## daemon metrics subscription

`metrics` request は TUI が daemon の観測用 stream を登録または解除するための control
vocabulary である。`subscribe` は TUI 起動時および接続を回復した後に送り、正常終了時には
`unsubscribe` を送る。接続が切れた subscription は connection-local であり、再接続で resume
せず新しく登録する。

daemon が送る snapshot は次の versioned schema である。これは表示・診断専用で、TUI が
session / terminal の所有権や local fallback を判断する根拠にはしない。

| field | type | meaning |
|---|---|---|
| `schema_version` | `u16` | metrics payload schema version。現在は `2` |
| `sampled_at_ms` | `u64` | daemon が sample を作成した monotonic timestamp |
| `cpu_percent_hundredths` | `u32` | 前回 sample からの daemon process CPU 使用率（百分率の 1/100 単位） |
| `resident_memory_bytes` | `u64` | daemon process の peak resident memory（byte） |
| `active_subscribers` | `u32` | sample 作成時の observer 数 |
| `dropped_updates` | `u64` | slow observer の bounded queue で coalesce した update 数 |
| `terminal_dropped_bytes` | `u64` | retention window から trim した terminal output byte 数 |
| `terminal_coalesced_bytes` | `u64` | retained segment に連結した terminal output byte 数 |
| `terminal_backpressured_bytes` | `u64` | bounded PTY observation queue の空きを待った terminal output byte 数 |

各 subscriber は容量 1 の queue を持つ。daemon は tick で block せず、queue が埋まった
observer の中間 sample を落として count する。切断された observer は次の publish で取り除く。
このため遅い TUI や一つの接続の切断が daemon tick または他 TUI の配信を止めない。

## PR inventory snapshot

`pr` request は stable `SessionId` を対象に daemon-owned inventory の source-of-truth snapshot を返す。
handshake では `pr.snapshot.v1` capability を必須にし、dedicated subscription を提供する peer は
`pr.subscription.v1` も advertise する。

| action / event | fields | contract |
|---|---|---|
| `snapshot` | `session_id`, `revision?` | canonical URL、optional title、state、pin/dismiss と refresh state を含む current snapshot を返す |
| `subscribe` / `unsubscribe` | `session_id` | connection-local hint subscription を登録・解除する。disconnect は登録を回収する |
| `pr.updated` | `session_id`, `revision` | inventory mutation を示す lossy hint。client は snapshot を再取得して収束する |

revision は session ごとに monotonic である。duplicate、欠落、順序逆転した `pr.updated` は client state
の差分適用根拠にしない。client は最後に見た revision より新しい hint を受けた場合、または reconnect 後に
snapshot を読み直す。slow subscriber は bounded queue で coalesce/drop され、PR refresh、terminal drain、
他 client の RPC を停止させない。

## managed session request

`session` kind の `create`、`remove`、`list`、`overview`、legacy `resume_agent` は daemon が所有する durable lifecycle / Agent runtime に届く。create / remove / resume_agent は producer-issued `OperationId` を accepted response に返し、list / overview は同じ revision 付き workspace snapshot を返す。create / remove の accepted response は snapshot とともに safe final hook を返す。hook は `kind`（`session.created` または `session.removed`）、`operation_id`、`revision` を持ち、TUI は create skeleton を同じ operation の `session.created` hook でだけ終了する。`OperationId` の再送は action と canonical session target が一致するときだけ同じ operation を返し、異なれば `idempotency_conflict` で拒否する。

create / remove の durable outcome と wire response / hook の対応は次の表を正本とする。同じ semantic operation の再送は daemon restart の前後を問わず同じ行を replay し、filesystem / Git effect を再実行しない。

| durable outcome | IPC outcome | final hook |
|---|---|---|
| `succeeded` | `accepted`（同じ `operation_id` / final revision / snapshot） | create は `session.created`、remove は `session.removed` |
| `failed`（effect failure または interrupted reconcile） | safe `error` | なし |
| 同じ `OperationId`、異なる action / canonical target | `idempotency_conflict` | なし |

snapshot の session は `WorkspaceId`、`SessionId`、`WorktreeId`、lifecycle を含み、workspace 全体の **root `WorktreeId`**（`⌂ root` の scope 識別子）も含む。agent / terminal 起動用の checkout path は、daemon が available の完全一致 scope（managed session、または `session_id` を持たない workspace root）からだけ解決する。client が name または path を渡して scope を再探索する wire contract はない。

## agent launch request

`agent` kind は daemon 所有の Agent runtime に届く。client は producer-issued `OperationId` と、`WorkspaceId` / optional `SessionId`（省略時は workspace root）/ optional profile ID だけの launch intent を送る。worktree、checkout path、profile 既定値、argv、environment、secret は wire field ではなく、daemon が [managed session scope](05-daemon.md#authority-と-lifecycle) と code-defined adapter registry から解決する。profile を省略すると daemon の既定 policy が選ぶ。

daemon は intent の `(WorkspaceId, SessionId?)` を [available scope](05-daemon.md#authority-と-lifecycle) の完全一致に解決し、その worktree だけを launch に使う。`SessionId` を省略した intent は workspace root に解決し、cwd を trusted repository root にする。creating / deleting / failed / stale / mismatch の scope、未知 profile、canonical でない `OperationId` は PTY を spawn せず typed safe error になる。

成功した launch は accepted response に producer `OperationId` と durable revision を返し、body に完全な `TerminalRef` と新しい `AgentContinuationRef` を載せる。この `TerminalRef` は operation・workspace・session・worktree・daemon generation・terminal incarnation を fence する。PTY exit を daemon が一度だけ記録すると、同じ semantic intent の再送は成功時に `completed: true` と同じ `TerminalRef` を持つ final response を返す。non-zero exit は安全な `unavailable` final として replay される。同じ `OperationId` を異なる intent で送ると `idempotency_conflict` になる。spawn failure・ambiguous・persist-after-spawn は fenced safe failure（`unavailable` / `ownership_unknown`）として durable に記録され、resend は同じ安全な失敗を replay する。replacement spawn や terminal の推測は行わない。

この replay 契約は daemon restart をまたぐ。fresh daemon は Agent snapshot の load、generation coordinator と operation ledger の hydrate、新しい process-local generation の atomic activate が完了するまで request admission を開始しない。旧 daemon process はその前に終了しており、PTY master は移送されない。`agents.json` は runtime record と generation/terminal ownership を同じ snapshot に持ち、admission、terminal command、exit、completion はすべてこの process-local authority を通る。restart 時に所有権を証明できない未終端 runtime は `identity_unknown` として inventory に `live: false` で現れ、旧 `TerminalRef` の command と late outcome は effect なしで拒否される。runtime と ownership binding の不一致、破損、未知 schema は daemon startup を fail closed にし、Agent spawn と snapshot 更新を行わない。schema v1/v2 は既存 runtime fence を保持した `identity_unknown` へ保守的に移行する。MCP caller credential は replay 対象ではなく restart で失効する。

```text
Agent request / PTY observation / completion
                  |
                  v
       GenerationCoordinator (process-local authority)
        | active generation admission
        | exact TerminalRef control/exit
        | exact CompletionFence outcome
                  |
                  v
 agents.json = generation ownership + runtime records (atomic)
```

Agent の pending pane は、同じ `OperationId` の成功 final が返した `TerminalRef` にだけ attach する。attach 以降の stream（`attach` / `resume` / `resync` / `input` / `resize` / `detach`）は [generic terminal request](#generic-terminal-request) と同じ vocabulary を共有し、daemon は `TerminalRef` の所有元（agent または generic）へ透過的に routing する。この pending pane の attach policy は [3. TUI](03-tui.md) を正本とする。

## Codex structured capture request

`codex_session_capture` kind は、daemon が Codex の `SessionStart(startup)` command hook にだけ注入する
private request である。documented hook JSON の current `session_id` と、同じ process provision にだけ存在する
daemon-minted credential を持つ。client は runtime / session / provider / path を指定できず、daemon は credential
から exact live Codex runtime を逆引きして structured capture 境界へ渡す。成功 response は body を持たず、
provider ID を返さない。

credential の欠落・不一致・失効、hook event / JSON / provider ID の不正、runtime の非 live、永続化失敗は safe error
であり、metadata を作らない。request の native ID はこの capture の入力でだけ一時的に IPC を通り、通常の Agent /
session request、response、event、status projection、error detail には現れない。hook input の `transcript_path` は wire field
に変換せず、file を開かない。capture と durable resume の正本は
[Provider-native conversation resume](05-daemon.md#provider-native-conversation-resume) とする。

## provider conversation resume request

`agent_inventory` は workspace root と全 managed session の live / interrupted history を同じ
`AgentInventory` として返す。live runtime item は完全な `AgentRuntimeRef`、
`AgentContinuationRef`、runtime state、optional source relation を持つ。resumable item は runtime ごとに
`available` と provider ID を含まない closed enum の safe reason を持ち、現 schema の record には
`AgentResumeTarget` を載せる。旧 record は `target: null` / unavailable のまま読み、identity を推測しない。
item は durable operation timestamp と stable runtime ID で決定的に並ぶため、同じ scope の複数 history
や Claude / Codex の混在を別 item として保持する。現 schema に `complete` / retention watermark はなく、inventory の
欠落は `AgentContinuationRef`、TUI dismissal、slot の削除を認可しない。TUI open は terminal inventory の前後 snapshot と
この `AgentInventory` が coherent な場合だけ全量を適用し、partial / cross-RPC 不整合では pane restore 全体を retry する。
Agent history / exit history / dismissal の allocator・retention・GC は
[#526](../.usagi/issues/526-fix-daemon-terminal-agent-tombstone-retention-aggregate-bound-gc.md) の責務であり、この request は
削除 authority を返さない。

`ResumeAgent` は利用者が明示的に開始する provider conversation の再開である。payload は canonical
`operation_id` と inventory が返した `AgentResumeTarget` をそのまま持つ。target は次の public fence だけで
構成する。

| field | fence |
|---|---|
| `continuation` | provider conversation lineage |
| `source` | resume source record の opaque incarnation |
| `workspace_id` / `session_id?` / `worktree_id` | root または managed session の exact scope |
| `runtime_id` | source runtime incarnation |
| `adapter_revision` | capture と resume adapter の互換 revision |

provider-native ID、provider 種別、cwd、profile argv、environment、transcript、旧 `TerminalRef` は target
にも他の client payload にも含めない。client は target を加工せず返し、daemon が durable record と全 field
を完全一致で検証する。したがって CLI、TUI、MCP は provider ID、名前、path、PID から target を再構成しない。
成功時は daemon が新しく所有する `AgentRuntimeRef` / PTY incarnation の完全な `TerminalRef`、同じ
`AgentContinuationRef`、`source` から replacement runtime / terminal への relation を返す。これは旧 PTY の
stream `resume` や再 attach ではない。

同じ operation と同じ exact target の再送は daemon restart 前後で同じ final と relation を replay し、別 target
への再利用は `idempotency_conflict` になる。double click 等で別 operation が同じ exact target を再送しても、
durable source → replacement relation から同じ final へ収束し、source は一度だけ supersede する。scope、worktree、
runtime incarnation、adapter revision、lineage の不一致、live source、metadata 欠落、provider unavailable は spawn 前に
safe な typed failure となる。native ID は
inventory、IPC、hook、error、log へ出さない。provider capture、fence、redaction、new PTY spawn の正本は
[Provider-native conversation resume](05-daemon.md#provider-native-conversation-resume) とする。

現 wire generation の互換期間だけ `SessionAction::ResumeAgent` の session ID / name 指定を受け付ける。
daemon がその scope の eligible exact target を厳密に 1 件へ解決できる場合だけ exact request に変換し、0 件は
`unavailable`、複数件は typed conflict で拒否する。最新 timestamp や provider 種別による暗黙選択はしない。
この legacy form は次の incompatible wire generation で削除できる。CLI の `resume-exact`、TUI の exact
resume port、MCP `session_resume` の `target` form は同じ exact contract を使い、inventory は CLI / TUI port と
MCP `agent_resume_inventory` が共通 contract を使う。

daemon restart、TUI 起動、workspace open 時の pane 復元は `ResumeAgent` を送らない。exact / legacy の
いずれも利用者による明示操作だけが request を作る。

## dispatch request

`dispatch` は managed session の既存 create lifecycle と Agent launch を合成する即時実行 request である。payload は producer-issued `operation_id`、workspace、session name、execution context から得た caller、排他的な worker selector（既存 `agent_id` または `runtime` と `model`）、prompt を持つ。daemon は session を reuse/create して available scope を確認してから、prompt を `initial_prompt` として launch する。成功 reply は Accepted outcome と `run_id`（operation ID）および fenced terminal を返す。同じ operation の再送は同じ outcome を返し、異なる intent は idempotency conflict である。

dispatch の operation key、caller↔worker binding、runtime generation、safe outcome も restart 時に hydrate される。同じ dispatch の retry は worker を再 spawn せず、保存済み outcome を replay する。

client は path、argv、queue/live mode、completion destination を指定しない。available でない session scope、agent selector の不整合、または未知 agent は safe typed error となり PTY を spawn しない。新規 agent の runtime/model は daemon が launch 直前に current workspace allowlist と current executable availability で再検証する。allowlist 外は `invalid_argument`、executable 不在は `unavailable` とし、どちらも PTY を spawn しない。

## generic terminal request

generic terminal の request vocabulary は `terminal` kind の `launch`、`inventory`、`attach`、
`resume`、`resync`、`input`、`resize`、`detach` である。launch は stable profile ID、
`WorkspaceId` / optional `SessionId` / `WorktreeId` の scope、geometry だけを送る。command、argv、
working directory、environment、secret は wire field ではなく、daemon が trusted profile から解決する。

launch の response は完全な `TerminalRef` を返す。attach は snapshot と connection-owned
subscription を同時に返す。input、resize、detach はその `TerminalRef` と subscription を必ず含める。
現行 launch response は immediate `Ok` であり、producer `OperationId` / Accepted revision / semantic replay を
持たない。daemon は request ごとに新しい terminal / operation identity を生成するため、spawn 後の response / ACK
loss をまたぐ launch 再送は同じ operation へ収束せず、二重 spawn し得る。client はこれを安全な mutation retry と
みなさない。durable reservation と replay は
[#518](../.usagi/issues/518-refactor-daemon-owner-generation-runtime-shard-global-resource-allocator.md) で追跡する。
terminal command の effect は、daemon generation、terminal、workspace、optional session、worktree、
runtime ownership/state の全 fence を read-only で検証した後だけ実行する。resize はこの preflight から
PTY effect、geometry commit まで terminal actor の排他区間を保持するため、途中の exit/replacement は
割り込まない。PTY effect が失敗した場合は `unavailable` を返し、committed geometry を更新しない。
output は `(start_offset, end_offset)` の連続範囲で表す。attach / resync snapshot は retention
window の先頭 `base_offset`、末尾 `output_offset`、その半開区間 `[base_offset, output_offset)` の
`replay` を返し、常に `base_offset + replay.length == output_offset` を満たす。window は最大 64 KiB
であるため、byte array の JSON 展開と response envelope を含めても既定 1 MiB frame 上限内に収まる。

resume は `after_offset` が window より古い場合、または `output_offset` より未来の場合に
`resync_required` を返す。window 内の segment
途中を指す場合は、その offset から始まる suffix を返し、最初の `start_offset` は必ず
`after_offset` と一致する。client は `resync_required` 後に snapshot で画面を置換し、返された
`output_offset` から resume する。同じ古い cursor を再送しない。この `base_offset` は protocol
generation 1 revision 1 の additive field であり、revision 1 client は必須 field として検証する。

`stale_target`、`ownership_unknown`、partial write を含む安全に証明
できない結果は typed error であり、client は local PTY を生成しない。

terminal input は daemon が PTY master に受理された byte 数を追跡し、operation の outcome として保持する。
同じ client の同じ `input_seq` と request identity を再送した場合は保存済み outcome を replay し、PTY へ再送しない。

| PTY write outcome | input ack | retry contract |
|---|---|---|
| 全 byte を適用 | `Written` | 同一 operation の再送は `Cached(Written)` |
| 適用済み prefix が 0 byte の failure | `Failed` | effect がないため、新しい operation として安全に再試行できる |
| 1 byte 以上を適用後の failure / `WriteZero` | `Ambiguous { applied_prefix }` | 同一 operation の再送は `Cached(Ambiguous { applied_prefix })` とし、既適用 byte を暗黙に再送しない |

PTY write が `Interrupted` を返した場合、daemon はそれまでの `applied_prefix` を維持して残りを再試行する。
wire 型は既存の `applied_prefix` を使うため protocol revision の変更を伴わない。

TUI adapter は final `ResponseOutcome::Ok` の ACK body だけを検証し、`Written` だけを通常成功として投影する。
Input response の `Accepted` はpendingであってfinal ACKではないため、bodyが見かけ上 `Written` でもeffect unknownとして拒否する。`Failed` / `Ambiguous` と
それらを包む `Cached` も daemon が input sequence を消費した final outcome なので、client は sequence を進めるが
subscription は切らない。`Ambiguous.applied_prefix` は `1..=input.length` だけを受理し、未知 variant、0 / 範囲外 prefix、
過剰に深い `Cached` は effect unknown として fail closed にする。

terminal Input の protocol error は `side_effect: none` の場合だけerror codeをdefinitive failureへ写せる。
`partial_or_unknown` / `applied` / `operation_accepted` はcodeにかかわらずeffect unknownであり、「未配送」と表示せず
connectionを捨て、blind replayしない。

request の write を試みた後で EOF / transport failure になった場合、client は PTY effect の有無を証明できない。
この ACK-loss 経路は「未配送」へ変換せず delivery unknown と表示し、同じ bytes を自動再送しない。現行の
connection-local ledgerだけでは reconnect 後に final outcome を照会できないため、cross-connection replayは別契約として扱う。
ACK lossと`Ambiguous`のuncertaintyはreattach成功や後続`Written`でclearせず、複数件をbounded count + first/latestで
集約する。現行UIからclearせず、session破棄または#519のdurable outcome resolutionまでlatchする。後続の
fatal/transport errorはprior uncertaintyを隠さず、current stateと合成して投影する。

`inventory` は `WorkspaceId` / optional `SessionId`（None=root）/ `WorktreeId` の scope を送り、
その scope に**完全一致**する daemon 所有 runtime を列挙する。daemon は generic terminal owner と
Agent owner の両方に問い合わせて結果を merge するため、応答には**generic terminal と Agent terminal の
両方**が含まれる。各エントリは完全な `TerminalRef`、`kind`（`terminal` / `agent`）、`live`（現 daemon
generation が所有し attach 可能か）だけを持ち、argv・environment 値・secret・provider transcript は
含めない。`exited`・reconcile 中・orphan の runtime は `live: false` として返り attachable にはならない。
client はこの列挙で発見した live runtime にだけ、その `TerminalRef` で fenced に attach する
（名前や path から terminal を推測しない）。workspace open 時の pane 復元でこの列挙を使う（[3. TUI](03-tui.md#workspace-open-時の-pane-復元) を正本とする）。

daemon restart 後も `inventory` は `terminals.json` から復元した generic terminal record を同じ scope と
`TerminalRef` のまま返す。ただし旧 daemon の PTY master は復元しないため、未終端 record は
`identity_unknown`、`live: false` となる。旧 ref の attach、resume、resync、input、resize、detach は
typed safe error となり、別 terminal の PTY effect や暗黙の replacement spawn を起こさない。restart 時の
永続化・破損時の扱いは [5. daemon](05-daemon.md#daemon-data-directory) を正本とする。

## Unix transport

Unix socket は daemon 専用 adapter が管理する。endpoint は private data directory の generation
directory に作り、bind 成功後に current locator を atomic publish する。directory は `0700`、socket と
locator は `0600` で、所有 UID・mode・symlink でないことを discovery と accept の両方で検証する。現行
`SecureUnixListener::bind` は endpoint bind と current publish を一つの処理で行うため、active locator を変えずに
private standby endpoint だけを ready にする段階はない。

current locator の publish と retire は owner-only の `current.lock` で直列化する。listener owner は
自分が publish した `(generation, endpoint)` と現在の locator が一致する場合だけ `current.json` を
unlink し、自 generation 固有の socket だけを回収する。したがって stale generation の遅延 retire / Drop が
replacement generation の locator または socket を削除することはない。planned generation end は accept loop の
停止・join 後にこの retire を完了し、client discovery を `NotFound` へ戻す。
locator publish は private temporary を fsync して atomic rename する。publish が commit 前に失敗した generation は、
まだ owner object が構築されていなくても自 socket と返却エラーの temporary の rollback を試み、rollback failure も
error として返す。process crash 後の stale endpoint / temporary recovery は #515 が扱う。

accept 時は OS peer credential の UID が daemon UID と一致しなければ、protocol byte を読む前に接続を
閉じる。client は active locator だけを解決でき、draining locator や generation directory 外を指す
endpoint には接続しない。cross-process standby handoff と draining owner routing は未実装であり、前者は
[#516](../.usagi/issues/516-refactor-daemon-cross-process-generation-registry-standby-handoff-authority.md)、後者は
[#508](../.usagi/issues/508-fix-tui-ipc-draining-generation-inventory-terminalref-owner-routing.md) で追跡する。到達不能な
draining resource を作る intermediate main を許さないため、shipping rollover は #508 の owner-generation routing
capability と compatible registry revision を確認できるまで disabled とする。最終 enable / restart / product E2E は
[#507](../.usagi/issues/507-fix-daemon-planned-restart-active-draining-generation-rollover.md) が担当する。

## client の失敗処理

TUI、CLI、MCP は共通 daemon client port を通して managed session と terminal の要求を送る。接続失敗、
protocol error、ownership unknown は local managed PTY や local session mutation への fallback を許可しない。

retry は `ProtocolError` の retry mode に従う。`OperationId` を持つ mutation を再送するときは元の operation
identity を保持する。generic Terminal Launch は現行 wire に producer `OperationId` がないため、この durable retry
契約の対象外である。TUI は stream sequence、resource revision、terminal output offset を別々に保持し、gap や
epoch の不一致では output を継ぎ足さず、snapshot resync を要求する。

terminal の `unavailable` は TerminalSession の connection-local subscription の喪失として扱う。TUI は
100ms から 2s 上限の指数 backoff 後、元の完全な `TerminalRef` に `attach` して atomic snapshot と
新しい subscription を取得する。transport EOF はclient connectionをdropして次回に開き直すが、
response bodyのlocal decode failureは同じclient connectionとinput ledgerを保持する。成功後は snapshot の
`output_offset` から `resume` し、backoff と subscription-local input sequence を、client-local connection epochが
変わった場合だけresetする。同じconnectionでのsnapshot reattachは
subscriptionが変わってもnext input sequenceを保持する。`stale_target`、`ownership_unknown`、exited は retry 対象ではなく、detach / tab
close も pending retry を解除する。どの失敗経路も replacement launch を行わない。

terminal input は Live な connection-owned subscription がある場合だけ送る。非 Live、subscription 不在、または
request を書く前の definitive failure は typed failure であり、client は success として捨てず未配送 feedback を表示する。
request write 後の transport / ACK loss は effect unknown として区別し、未配送と断定しない。どちらも再接続まで入力を
queue / replay せず、unknown inputをblind retryしない。

MCP の dispatch request は `DispatchTool` action として送る。daemon が session upsert、agent/run/binding
の解決、inbox の読み書きを行い、MCP は durable state を直接読んだり書いたりしない。完了・失敗は worker
の current run と binding が一意に一致するときだけ配送し、不一致は completion fence と同じ fail-closed
方針で no-op にする。
