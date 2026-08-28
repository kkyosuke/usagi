# 18. Director mode と複数 workspace の境界

> [設計提案一覧](README.md) ｜ 関連仕様: [TUI](../03-tui.md) ｜ [daemon](../05-daemon.md) ｜ [MCP](../07-mcp.md) ｜ 関連提案: [multi-workspace daemon](17-multi-workspace-daemon.md) ｜ [workspace-root scope](10-workspace-root-scope.md)

1 daemon が複数 workspace を tenant として serve しても、**Director mode の実行権限は選択中 workspace の
root scope に閉じる**。multi-workspace は workspace を止めずに切り替えるための daemon 能力であり、adopt 済み
workspace 全体を 1 人の Director が暗黙に操作できるという意味ではない。

本提案は、現行の workspace-local Director を維持したまま複数 workspace と組み合わせる挙動と、将来
workspace 横断面が必要になった場合の境界を定める。

## 目次

- [決定](#決定)
- [3 つの scope](#3-つの-scope)
- [workspace の切り替え](#workspace-の切り替え)
- [表示と状態の分離](#表示と状態の分離)
- [daemon 全体で共有するもの](#daemon-全体で共有するもの)
- [横断ビュー](#横断ビュー)
- [Portfolio Director](#portfolio-director)
- [security invariants](#security-invariants)
- [却下した案](#却下した案)
- [段階](#段階)
- [test 戦略](#test-戦略)

## 決定

| 問い | 決定 |
|---|---|
| Director conversation の scope | workspace-local。`(workspace_id, session_id: None, worktree_id)` で fence する |
| drawer に出す組織 | 現在 Home で開いている workspace の Organization tree だけ |
| workspace 切り替え時の root Agent | TUI は detach するが daemon-owned runtime は停止しない。戻ったときにその workspace の intent / inventory から復元する |
| 別端末からの同時利用 | TUI process ごとに 1 workspace。複数 process が同じ daemon の別 tenant を同時に開ける |
| workspace 横断の観測 | Director drawer へ混ぜない。必要なら Welcome-level の read-only な Portfolio view として分ける |
| workspace 横断の実行 | tenant adoption や workspace 登録から権限を推測しない。将来必要なら別の `Portfolio Director` scope と明示 grant を導入する |

Director drawer は「daemon の管理画面」ではなく、「workspace root の Agent conversation 面」である。この定義を
保つことで、root sandbox、MCP caller credential、dispatch binding、inbox、workspace settings / role catalog の
権威を変更せずに multi-workspace daemon と組み合わせられる。

## 3 つの scope

複数 workspace 対応では、次の 3 層を同じ概念として扱わない。

```text
daemon scope                    machine / data directory
├─ workspace A scope            tenant A / root Director A
│  ├─ managed session A1
│  └─ managed session A2
└─ workspace B scope            tenant B / root Director B
   └─ managed session B1
```

| scope | 所有するもの | 所有しないもの |
|---|---|---|
| daemon | generation、allocator、全体 capacity、tenant registry | repository の作業方針、root conversation |
| workspace | root worktree、settings、role catalog、root Director、session 集合 | 他 workspace の session / inbox / filesystem |
| managed session | task worktree、割り当て role、session Agent | workspace root や sibling session の追跡ファイル |

`session_id: None` は「daemon root」ではなく、**ある workspace の root** を表す。したがって root identity を
比較・保存・投影するときは `workspace_id` を省略できない。別 workspace の root 同士を `None == None` として
同一視してはならない。

## workspace の切り替え

[workspace の離脱と終了](../03-tui.md#workspace-の離脱と終了)の契約をそのまま使う。

```text
Home(A) + Director A
  -> Welcome へ離脱
     TUI: Director A を detach、A の lane / port / pump を teardown
     daemon: Director A の runtime と durable operation は継続
  -> workspace B を選択して handshake
     TUI: B 専用の lane / intent / inventory を開始
  -> Home(B) + Director B
```

- A の drawer open/closed、conversation 選択、scroll、picker draft は B へ引き継がない。durable な tab order / selection
  だけが workspace ごとの `AgentTabIntent` に従って A を再 open したときに復元される。
- A で launch 中の operation が離脱後に完了しても B の pending slot へ投影しない。A の inventory / intent reconciliation
  が次回 open 時に回収する。
- B に root conversation が無ければ empty state を表示する。A の conversation を fallback 表示しない。
- 同じ TUI process は一度に 1 workspace とだけ接続する。A と B を同時に操作したい場合は 2 つの TUI process を使う。

この挙動では workspace 切り替えが Agent の終了を意味しない一方、入力先は常に画面 breadcrumb が示す 1 workspace に
限定される。

## 表示と状態の分離

Director drawer の projection は handshake で解決した workspace の snapshot だけを入力にする。

| 情報 | key / fence | 別 workspace の入力を受けた場合 |
|---|---|---|
| root conversation | `workspace_id` + `TerminalRef` / continuation | 投影しない |
| Organization tree | `workspace_id` + session / dispatch binding | row を作らない |
| pending operation | `workspace_id` + root `worktree_id` + `OperationId` | completion を拒否する |
| user decision | `workspace_id` + target identity | notice / modal に出さない |
| tab intent | `<data-dir>/tui/workspaces/<workspace-id>/...` | current registry へ merge しない |

UI の title は `♛ Director` を維持し、Home header の workspace breadcrumb を scope 表示の正本とする。同じ drawer 内に
workspace selector を追加しない。drawer だけを別 workspace へ切り替えると、背景 Home、session sidebar、decision、
terminal connection の scope と入力先が食い違うためである。

## daemon 全体で共有するもの

daemon-wide な事実は workspace-local Director から観測してよいが、workspace-local な事実に見せかけない。

| 事実 | 扱い |
|---|---|
| Agent concurrency | daemon 全体の使用中 / 上限を表示する。他 workspace の消費を current workspace の Agent 数として数えない |
| daemon health / metrics | daemon 全体の診断値として表示する。workspace の lifecycle state に変換しない |
| tenant 上限 | 新しい workspace の open / adopt 時にだけ判定する。Director launch の workspace authorization には使わない |
| shutdown / restart | 全 tenant に影響する daemon 操作。Director drawer の会話操作として提供しない |

capacity 到達で Director launch が拒否された場合は「daemon の Agent capacity が満杯」であることだけを安全に示す。
別 workspace の path、session 名、Agent prompt を理由へ含めない。

## 横断ビュー

複数 workspace の進捗を 1 画面で見たい要求は、workspace-local Director の拡張ではなく **Portfolio view** として扱う。
置き場所は workspace をまだ選択していない Welcome-level とし、最初は read-only に限定する。

| Portfolio view が表示できるもの | 条件 |
|---|---|
| registered workspace の名前 | global workspace registry に存在する |
| running / waiting / failed の集約件数 | daemon が workspace-fenced な safe projection を返せる |
| root Director の live / interrupted 有無 | provider ID、prompt、inbox 本文を含まない |
| daemon 全体 capacity / health | daemon-wide な値として明記する |

Portfolio view は tenant registry を workspace 一覧の権威にしない。tenant は daemon が現在保持している実行資源であり、
登録 workspace は利用者が開ける対象だからである。未 adopt workspace を一覧表示するためだけに adopt せず、一覧から
消えた tenant を workspace 削除と解釈しない。

row を選んだ後の実行操作は、その workspace を通常どおり open して handshake を完了してから Home / Director で行う。
Portfolio connection から session create、prompt 送信、decision resolve、terminal attach を直接行わない。

## Portfolio Director

将来「1 人の Director Agent が複数 repository の計画を調整する」必要が明確になった場合は、既存 root Director を
拡張せず、別の `Portfolio Director` scope として設計する。この機能は本提案では実装しない。

最低限、次の契約が必要になる。

| 必要な契約 | 理由 |
|---|---|
| 明示的な workspace grant set と revision | adopt 済み / registered であることは Agent への操作許可ではない |
| workspace 非所属の coordinator identity | 既存 `session_id: None` は workspace root を意味し、横断 identity に再利用できない |
| target workspace を必須にした dispatch | cwd、settings、role catalog、runtime/model allowlist を対象 workspace から解決する |
| child は常に 1 workspace に束縛 | 1 worker に複数 root の writable access を与えない |
| `(workspace_id, run_id)` を持つ report / inbox | 同名 session、遅延完了、grant 失効後の報告を混同しない |
| workspace ごとの部分失敗 | B の fence / settings failure で A の dispatch や観測を巻き戻さない |

```text
Portfolio Director（repo 外、明示 grant: A/B）
├─ dispatch target=A -> workspace A の Manager / Worker
└─ dispatch target=B -> workspace B の Manager / Worker
```

Portfolio Director 自身は repository の追跡ファイルを編集しない。実装・issue 更新・PR は target workspace に作成した
managed session へ委譲する。grant の追加は利用者の明示操作、縮小は新規 dispatch を即時拒否し、既に走っている child の
完了報告は origin workspace を保持したまま read-only に回収できる、という revoke semantics を別途決める。

## security invariants

- daemon が workspace を adopt した事実から、Director Agent の read / write / dispatch 権限を導出しない。
- handshake で選ばれた `workspace_id` を list / get / inventory / inbox の filter と effect の fence の両方に使う。
- root sandbox の writable root は current workspace だけとし、別 tenant の root や session worktree を追加しない。
- role は prompt policy であり authorization ではない。`Director` role を選んだだけで workspace 境界を越えない。
- 別 workspace の identity を not-found として扱う現行 MCP 契約を維持し、存在の oracle を作らない。
- TUI は wrong-workspace な final / event / cached intent を current workspace へ fallback しない。
- daemon-wide aggregate は path、session 名、prompt、inbox 本文を含まない safe projection に限定する。

## 却下した案

| 案 | 却下理由 |
|---|---|
| Director drawer に workspace selector を足す | 背景 Home と drawer の scope が分かれ、入力・decision・completion の宛先を誤認しやすい |
| daemon に root Director を 1 つだけ持つ | workspace settings / role catalog / sandbox / inbox の権威を選べず、`root` の意味を壊す |
| adopt 済み tenant を Director の許可集合にする | client が workspace を開いただけで Agent 権限が増える ambient authority になる |
| `unbound` connection から横断 effect を許す | workspace fence を迂回し、どの root / worktree が対象か証明できない |
| workspace 切り替え時に Director を移動する | conversation の cwd・sandbox・dispatch binding は生成元 workspace に束縛されており、安全に移し替えられない |
| 全 workspace root を 1 Agent の sandbox に writable で渡す | 誤操作の blast radius と credential exposure を拡大し、session 単一書き手も守れない |

## 段階

| 段階 | 内容 | 状態 |
|---|---|---|
| 0 | Director は current workspace に閉じ、切り替え時は detach / reopen で復元する | 現行仕様 |
| 1 | wrong-workspace event / intent / inventory の拒否と、A → B → A の復元を回帰 test で固定する | 既存 test の棚卸し後に不足分を issue 化 |
| 2 | Welcome-level の read-only Portfolio view を追加する | 利用者要求が明確になった時点で別 issue / 提案にする |
| 3 | 明示 grant 付き Portfolio Director を追加する | Portfolio view では足りない実行要求が確認された場合だけ別提案にする |

段階 2 と 3 は独立である。横断して見たい要求から、横断して実行する権限を自動的に導入しない。

## test 戦略

| 層 | 検証 |
|---|---|
| TUI reducer | A の root conversation / completion / decision を B の drawer に投影しない。workspace 再 open で A の selection を復元する |
| TUI integration | Director A を live にしたまま Welcome → B を開き、Director B が空または B 固有の inventory になり、A が生存する |
| daemon / MCP | A credential で B の agent / session / inbox を list / get / dispatch できず、B の存在を漏らさない |
| sandbox E2E | Director A の writable root に B の root / session tree が含まれない |
| multi-client E2E | TUI A と TUI B が同じ daemon に同時接続し、それぞれの Director 入力・output・decision が混ざらない |
| capacity | A と B の launch が daemon-wide limit を共有するが、拒否理由に他 workspace の metadata が出ない |
