---
number: 571
title: feat(tui): workspace root Agent を session 一覧から分離して右 drawer に昇格する
status: todo
priority: high
labels: [v2, tui, agent, ux, design, epic]
dependson: [575, 576, 577, 578, 579]
related: [388, 506, 510, 545]
created_at: 2026-07-27T22:43:36.804576+00:00
updated_at: 2026-07-27T23:05:42.278193+00:00
---

## 背景・現状

現在の Home は `Target::Root(WorkspaceId)` を表示名 `main` の 1 行として左 sidebar の先頭に置き、managed session と同じ Closeup/pane surface で扱う。`main → divider → session* → + new session` が navigation の正本で、root でも Agent / Terminal / Diff 等の pane を持てる。

しかし workspace root は並列作業用の managed session ではなく、workspace 全体を操作・相談するための常設 Agent surface として扱いたい。通常 session と同じ一覧・Closeup に置くと、次の概念が混ざる。

- 左一覧の「作業 session」と workspace root が同列に見える。
- session を切り替える操作と、workspace 全体の Agent を開く操作が同じ target selection になる。
- root に Terminal / Diff など、workspace Agent に不要な pane action が現れる。
- session が 0 件のときも hidden fallback として root が active になり、通常 session 用 action が root に流れ得る。

既存実装には利用可能な基盤がある。root scope は wire 上 `session_id: None` として daemon が trusted repository root に解決し、root Agent の live/interrupted inventory、exact resume、`AgentTabIntent` の target (`session_id: None`) と選択・順序・dismissal、install 済み CLI の `AvailableModels`、explicit profile 付き `LaunchAgent` は既に存在する。本 issue はこれらを壊さず、TUI の情報設計と導線を変更する。

## 目的

- 左 sidebar を managed session の一覧に限定し、workspace root / `main` 行を表示しない。
- workspace root を **Workspace Agent** という workspace-global な Agent-only surface に昇格する。
- Workspace Agent は一般的なチャット UI のように、Home の右端から重なる overlay drawer として表示する。
- drawer を開いたとき、前回選択していた root Agent conversation を復元・選択する。open 自体は Agent の新規 spawn や provider resume を発火しない。
- `New` では install 済み Agent CLI を利用者が明示選択して、新しい root Agent conversation を開始できる。

## 用語・概念の決定

| 旧 UI 概念 | 新 UI 概念 |
|---|---|
| sidebar の `main` row | 廃止。workspace root は session row ではない |
| root Closeup | 廃止。root は Workspace Agent drawer からだけ表示する |
| `Target::Root` | daemon/core の workspace-root scope として維持する。表示上の session identity には使わない |
| root pane registry | root Agent conversation の保持・復元にだけ使う。Terminal / Diff / action palette は drawer に投影しない |
| managed session Closeup | 左一覧で選ぶ従来の作業 surface。対象は `SessionId` を持つ session のみ |

画面上の名称は `main session` ではなく **Workspace Agent** に統一する。Git branch の `main`、workspace root scope、daemon の `session_id: None` は別概念なので変更しない。

## 画面・導線

### 開く入口

primary entry は Home header 右側の `Workspace Agent` button とする。sidebar の外に置くことで「session の一つ」ではなく workspace-global な操作であることを示す。

keyboard entry は `Ctrl-O g` とする。

- Switch / managed-session Closeup のどちらからでも同じ drawer を開く。
- live pane 上では既存 `Ctrl-O` leader の follow-up として解決し、通常の Agent 入力を横取りしない。
- drawer 表示中の `Ctrl-O g` と `Esc` は drawer を閉じる。
- 他の modal / decision editor / create form が入力を所有中は背景の header click と shortcut を処理しない。

header button の click hit-test は terminal resize と Unicode display width に追従し、notice badge / mode toggle と重ならないよう同じ header layout authority から計算する。

### drawer layout

- Home の frame を背景に残し、背景を dim して右端から full-height（Home header の下）に重ねる。
- 通常幅では terminal 幅の約 60%、上限 96 columns、下限 56 columns を目安に clamp する。
- 狭幅で背景と drawer の双方を可読にできない場合は Home header の下を全幅で覆う。0/極小 geometry でも panic せず既存 normalize 規則へ縮退する。
- drawer 自体が最前面の input owner で、背景 sidebar、managed-session pane、header の他 action へ入力・click を伝播しない。
- close 後は、開く前の sidebar cursor、active managed session、Closeup mode、selected pane tab、scroll/selection をそのまま復元する。drawer を開閉しても managed-session target を root へ変更しない。

### drawer content

Workspace Agent drawer は Agent conversation だけを持つ。

```text
┌ Workspace Agent ───────────────────────────────┐
│ [previous conversation ▼]                [New] │
│                                               │
│ selected Agent terminal / interrupted state   │
│                                               │
│ Esc close   Ctrl-O n/p switch   Ctrl-O x close│
└───────────────────────────────────────────────┘
```

- live Agent は既存の VT terminal projection、input、resize、scroll、link/copy、detach/reconnect 契約を再利用する。provider transcript を別形式へ再構成する native chat renderer は作らない。
- interrupted Agent は既存の safe label/body と明示 `Ctrl-O r` resume を使う。
- conversation 切替、reorder、close/dismiss、reopen、resume は `AgentContinuationRef` / exact `TerminalRef` と root target の `AgentTabIntent` を正本にする。
- root scope の generic Terminal、Diff、pending Terminal、Terminal 起動 action は drawer に表示せず、作成経路も持たない。未リリース機能の後方互換は考慮せず、Workspace Agent の root pane は Agent-only を invariant とする。
- drawer から `terminal` / `diff` / session close / root close を起動できない。Agent-only 制約は表示だけでなく reducer/effect 境界でも検証する。

## 前回 Agent の選択と復元

workspace open 時に既存の coherent inventory restore と root `AgentTabIntent` を reconcile し、drawer の selected conversation を準備する。drawer は自動では開かない。

| 復元状態 | drawer を開いたときの表示 |
|---|---|
| saved selection が trusted live Agent と一致 | 同じ exact Agent tab を選択し、drawer foreground になった時だけ attach/resync |
| saved selection が interrupted/resumable history | 同じ interrupted conversation を選択。自動 resume しない |
| saved selection が消失し、同 root に別 conversation がある | intent の deterministic order における次の surviving conversation、なければ先頭 |
| root Agent conversation が無い | empty state と `New` affordance |
| daemon/inventory が一時不通 | last valid intent を破棄せず reconnecting/error を安全に表示。local spawn しない |
| corrupt state | 既存 `AgentTabIntent` の quarantine/empty rebuild 契約に従う |
| future schema | read-only notice を出し、bytes を上書きせず mutation/new launch を fail closed にする |

opening/reopening/reconnect/resize は launch や provider resume の理由にならない。選択済み live Agent が別 TUI client でも継続している場合も exact identity へ attach し、spawn count を増やさない。

## `New` flow

`New` は drawer 内の明示的な Agent CLI picker を開く。

- 候補は `AvailableModels` に含まれる install 済み CLI のみ（現 vocabulary: `claude` / `codex` / `sakana.ai`）。
- config の `default_model` を初期 highlight にするが、利用者が CLI row を選んで Enter するまで launch しない。
- picker は Cancel / Esc で conversation と選択を変更せず閉じる。
- 確定時は `Target::Root(workspace)` / `session_id: None`、新しい `OperationId`、選んだ explicit profile で既存 daemon Agent launch path を 1 回だけ呼ぶ。
- pending conversation を 1 枚だけ追加し、matching operation と root scope を持つ successful final の exact `TerminalRef` だけを live に昇格する。
- double Enter、duplicate accepted/final、reconnect replay は operation fence で 1 spawn / 1 tab に収束する。
- install 済み CLI が 0 件なら picker を空で開かず、設定/installation を促す safe empty state を表示する。
- configured default が未 install の場合は最初の install 済み候補を highlight するだけで、自動確定しない。
- daemon 不通、profile rejection、stale/wrong-scope final、persist failure は既存 conversation と selection を壊さず safe error にする。argv、cwd、provider-native ID、raw daemon error は表示・保存しない。

## sidebar / controller state の変更

sidebar rows は `session* → + new session` とし、root row と直後の divider を削除する。

- session がある初期表示は、復元可能な selected/active `SessionId` があれば保持し、無ければ先頭 session を cursor/active の候補にする。
- session が 0 件のときは `+ new session` を選択し、通常 Closeup の active session は `None` とする。hidden root fallback から Terminal/Agent action を実行しない。
- active session が snapshot から消えた場合は、表示順の近い surviving session、無ければ `+ new session` へ安全に着地する。Workspace Agent drawer の root conversation state は影響を受けない。
- managed session 作成成功後の auto-landing、remove、double-click、scroll viewport、mascot reservation、pending skeleton は root row が無い geometry を正本にして更新する。
- managed-session Overview/session command と workspace-global Overview/config/env/decision の既存責務は維持する。Workspace Agent drawer は Overview の代替ではない。

必要であれば `AppState.active` を `Option<SessionId>` 相当の managed-session focus と workspace drawer state に分離する。`Target::Root` を通常 sidebar の fallback として残して表面だけ隠す実装は不可とする。

## input / terminal ownership

- drawer が閉じている間、root Agent tab は background/detached のまま daemon で継続する。
- drawer が開いたときは選択中 root Agent だけを foreground attach/resync し、他 root conversation と全 managed-session pane は detached/background とする。
- drawer を閉じると root foreground subscription を detach し、開く前に foreground だった managed-session tab を attach/resync する。PTY/process は kill しない。
- drawer geometry を root Agent PTY の viewport resize に使う。背景右 pane の幅を誤って送らない。
- stale delayed restore/attach/resize/input ACK は既存 pane-registry revision、interaction、subscription/sequence fence で current drawer selection を上書きしない。

## 実装範囲

本 issue は全体設計と最終受入を管理する Epic とし、実装は次の子 issue に分割する。

| 順序 | Issue | 所有する責務 |
|---|---|---|
| 1 | #575 | sidebar / navigation から root target を分離し、session 0 件を含む managed-session state を整理する |
| 2 | #576 | header entry、`Ctrl-O g`、右 overlay drawer shell、geometry、入力 ownership を追加する |
| 3 | #577 | root Agent conversation の durable restore、Agent-only pane、foreground attach / input / resize / explicit resume を接続する |
| 4 | #578 | install 済み CLI picker と explicit profile による新規 root Agent launch を追加する |
| 5 | #579 | shipping TUI / daemon / PTY E2E、旧 root Closeup 経路の除去確認、仕様ドキュメント更新を完了する |

依存順は `#575 → #576 → #577 → #578 → #579` とする。各 issue は直前までの public seam を使い、後続責務を
先行実装しない。Epic #571 は全子 issue が完了し、下記の横断受入条件が満たされた時点で完了する。

1. **controller/state**
   - root を sidebar selection/active fallback から分離する。
   - `WorkspaceAgentDrawer` の open/close/picker/empty/error state と effect を追加する。
   - Agent-only effect admission、background overlay precedence、0-session landing を pure reducer で固定する。
2. **presentation**
   - sidebar から `main`/divider を削除し viewport/click geometry を更新する。
   - header button と right-aligned drawer、狭幅 fallback、drawer-local conversation selector / New picker を実装する。
3. **pane/runtime**
   - root Agent registry/intent/inventory を drawer に接続し、root pane に generic Terminal / Diff を作成できない invariant を設ける。
   - drawer foreground attach、close 時の managed-session foreground restore、drawer geometry resize を実装する。
4. **input**
   - `Ctrl-O g` と header click を同じ open/close action へ束縛し、live Agent、Switch、Closeup、overlay precedence を統一する。
5. **launch/persistence**
   - `AvailableModels` picker から explicit root Agent launch を行い、root `AgentTabIntent` の order/selection/dismissal を既存 atomic/CAS 契約で保存する。
6. **docs**
   - [TUI](../../document/03-tui.md) の Home/target、sidebar、Closeup、pane restore、agent CLI 選択を新概念へ更新する。
   - 必要なら architecture/IPC/daemon docs は「wire/root authority は不変で UI surface のみ変更」と明記し、同じ事実を重複記載しない。

主な変更候補は `crates/tui/src/usecase/application/controller.rs`、`pane.rs` / `pane_runtime.rs`、`presentation/workspace_runtime.rs`、`presentation/views/workspace.rs`、`presentation/mod.rs`、`usecase/terminal_input.rs`、合成ルートの TUI runtime と関連 E2E である。

## 受入条件

- [ ] 左 sidebar と click/hit-test/keyboard rows に `main` / root row / root divider が存在せず、managed session と `+ new session` だけが表示される。
- [ ] session 0 件でも hidden root Closeup に入らず、`+ new session` と Workspace Agent header entry の双方を利用できる。
- [ ] header button と `Ctrl-O g` は Switch / managed-session Closeup から同じ right drawer を開き、Esc / 再 chord で閉じる。背景 state と foreground tab は開閉前後で保持される。
- [ ] drawer は root Agent conversation だけを表示し、Terminal / Diff / close-root action を表示も dispatch もしない。
- [ ] TUI close/reopen 後に root の前回 selected live Agent が exact identity で選択され、PID/spawn count を増やさず retained output/input を継続できる。
- [ ] interrupted root Agent は前回 selection のまま表示され、drawer open/reconnect では resume せず、選択 tabへの明示 Resume だけが replacement を 1 回 spawn する。
- [ ] `New` は install 済み CLI の picker を必ず経由し、選択した explicit profile で root Agent を 1 回だけ起動する。未 install CLI は候補/補完/admission に現れない。
- [ ] root Agent の複数 conversation で selection/reorder/close/reopen が durable に保持され、duplicate/stale inventory と concurrent client CAS でも lost update・二重 tab・focus steal を起こさない。
- [ ] TUI から root generic Terminal / Diff を作成・復元・表示する経路が存在せず、root pane は Agent-only である。
- [ ] drawer open 中の daemon outage、wrong-scope final、persist failure、future schema、CLI 0 件を fail closed に扱い、既存 pane/intent/bytes を成功扱いで変更しない。
- [ ] 極小幅・resize・CJK workspace/session 名・notice badge ありでも drawer/header/sidebar の表示幅と click target が一致し、panic・style leak・誤 resize を起こさない。
- [ ] managed session の create/remove/Closeup、Overview/config/env/decision、pane restore、quit/Welcome への離脱に回帰がない。

## 必須テスト

### pure/reducer/presentation

- sidebar row ordering、0/1/N sessions、selection/active reconciliation、remove/create landing、pointer hit-test。
- drawer open/close と overlay precedence、managed foreground 保存、root Agent-only filter、narrow/full-width layout。
- previous live/interrupted/absent selection、duplicate inventory、stale delayed observation、dismiss/reopen、future/corrupt intent。
- New picker の installed/default/0 candidates、cancel、explicit profile、double submit、wrong-scope final、safe error rollback。
- `Ctrl-O g` classifier と Switch/Closeup/live root Agent の input ownership。

### integration / shipping E2E

shipping TUI、実 daemon process/socket/PTY、fixture Agent CLI を使い、最低限次を固定する。

1. managed session pane を foreground にしたまま drawer を開き、root Agent を CLI picker から起動する。drawer closeで managed pane、再openで同じ root Agentへ戻り、両 child PID/spawn count が不変である。
2. root に複数 Agent を作り、非先頭 selection/reorder/close を保存して TUI を終了・再openする。drawer は exact tab/order/selectionを復元し、root generic Terminal / Diff の作成 request は発行されない。
3. daemon cold restart 後は選択していた root history を interrupted として表示し、自動 resume 0、明示 `Ctrl-O r` 後だけ replacement spawn 1 になる。
4. session 0 件、CLI 0 件、daemon outage、duplicate final、resize/narrow terminal、drawer open 中の workspace leave を通し、local spawn・二重 tab・background input leakage が無いことを確認する。

既存 `tests/cli_tui.rs` / `tests/cli_tui_pty.rs` / `tests/agent_ipc_e2e.rs` と TUI unit harness を拡張し、reducer fake だけを product acceptance の代用にしない。

## 非目標

- provider transcript を parse して独自メッセージ UIへ変換すること。
- daemon/core の root scope、`session_id: None`、trusted repository root 解決を廃止すること。
- root Agent process を drawer close や tab dismiss で kill すること。
- MCP/CLI からの root terminal 能力をこの TUI redesign の副作用で削除すること。
- Workspace Agent を自動起動・自動 resume すること。

## 関連

- #506: root を含む Agent tab intent の durable order/selection/dismissal と inventory reconciliation
- #510: root / managed session の interrupted Agent tab と exact explicit resume
- #545: install 済み Agent CLI vocabulary、default、picker/completion、explicit profile launch
- #388 / workspace pane restore 系: daemon-owned live Agent/Terminal の attach・復元基盤
