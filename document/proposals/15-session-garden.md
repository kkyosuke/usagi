# 15. session garden

> [設計提案一覧](README.md) ｜ 現在仕様: [TUI](../03-tui.md) ｜ 実装履歴: #674

session を庭の区画、その agent を区画にいるうさぎとして表す Home の screen saver UI を導入した際の
設計記録である。現在の表示・入力契約は [3. TUI#session garden](../03-tui.md#session-garden) が正本であり、
本書は採用理由と実装履歴を残す。Garden の目的は session 数や実行状態を一覧表より速く把握できることと、
`usagi` らしさを操作の邪魔にならない範囲で強めることである。

Garden は session の lifecycle や Agent phase を所有しない。daemon-authoritative な既存 projection を
絵へ写すだけで、session の選択と操作は引き続き stable `SessionId` を使う。

## 画面案と遷移

Garden は Home の一時的な全幅レイヤーとして開く。常駐 route の Switch / Closeup は増やさず、背面の route、
active session、pane、terminal subscription を変えない。Garden を閉じると、表示前と同じ Home へ戻る。

既定の idle threshold は **5 分**とする。閾値と、どの入力が idle を延ばし・どの surface が自動表示を止めるかは
実装済みの仕様であり、[3. TUI#自動表示](../03-tui.md#自動表示) が正本である。ここでの設計判断は「idle を
*利用者の操作*だけで測る」ことにある。tick、daemon/backend event、Agent/terminal 出力を延長に数えないため、
Agent が動き続けていても人が操作していなければ Garden を表示できる。逆に未送信入力や destructive action の
確認は覆えないので、前面の overlay と Director drawer は自動表示を止める。

```text
 usagi / my-project                                      3 sessions · 1 running
 ──────────────────────────────────────────────────────────────────────────────

       .  *                    session-auth
              (\(\                 running
           ＿( o.o)    ぴょん
             > ^ <

                 🌱                         issue-647
       ──v────v──────────────             (\(\        waiting
                                            (o.o)?
                            coder          o(_(")(")
            (\(\          available
            ( -.-)
           o(_(")(")             ·  ·  ·                  failed-build
                                                          ×(x.x)
       --v-------v-----------v-----------v----------v-------v----------v-------
       Garden  ←/→ scroll · click to visit · Esc to return
```

絵文字は端末で 1 桁または 2 桁になり得るため、production の地面・草花は ASCII を基本にする。上の `🌱` は
雰囲気を示すモックであり、実装では幅が決定的な `*` / `.` / `v` を使う。うさぎ本体は個体ごとの palette、
nameplate は選択状態によらず dim、失敗は `Role::Danger` で描く。色を見分けられない場合にも状態ラベルと
顔の違いで状態を判別できる。

## うさぎは agent、区画は session

**1 区画（plot）= 1 session、1 うさぎ = 1 agent** とする。session は「場所」であり、実際に作業しているのは
agent なので、動くものを agent に対応させる。

初期案は 1 session = 1 うさぎで、controller が集約した Agent phase を 1 つだけ描いていた。1 session は
agent を複数持てる（controller の runtime 一覧を session で絞り込む）ため、この集約は次の 2 つを失う。

- **何羽いるか**。agent が 1 つの session と 5 つの session が同じ絵になる。
- **動いている agent**。集約は `Done > Waiting > Running > Ready > Absent` の最大ランクを採るため、
  「1 つ終了・1 つ実行中」の session は `Done` に畳まれ、**休んでいるうさぎとして描かれる**。実行中の作業が
  庭から消える。

この畳み込みは sidebar の 1 行要約としては妥当である（終わった agent は「見に来て」という知らせなので
最上位でよい）。しかし Garden の目的は実行状態を一覧表より速く把握することなので、ここでは逆効果になる。
したがって Garden は集約後の値ではなく、**session に属する agent ごとの phase** を描画素材にする。

見た目のために永続 session record や IPC event は増やさない。agent membership は coherent Agent inventory と
controller の runtime 一覧を stable identity で結合し、agent ごとの詳細な phase は controller が既に持っていれば
そちらを優先する。inactive project は `AgentWorkspaceObservation` で runtime detail と session-level dispatch status を
同時に読み、availability を推測しない。これにより TUI 起動前から存在する agent も Garden から欠落しない。

### 状態の対応

session の lifecycle は区画（nameplate と地面）が表し、agent の phase は個々のうさぎが表す。

| 区画（session lifecycle） | 庭での表現 |
|---|---|
| `Available` | 通常の区画。うさぎが居る |
| `Creating` | 土から耳だけ見える。agent はまだ居ない |
| `Failed` | 伏せて止まる。短い safe failure label を添える |
| `Deleting` | 奥へ帰る。位置は固定し、段階的に dim にする |

| うさぎ（agent phase） | 庭での表現 | animation |
|---|---|---|
| `Running` | 前へ跳ねる | 低い姿勢 → 空中 → 着地の 3 pose |
| `Waiting` | 座って首をかしげる | `?` と耳をゆっくり交互表示 |
| `Ready` / `Absent` | 草のそばで休む | ときどき瞬きする |
| `Done` | 座って待つ | animation しない |

agent が 1 つの session は 1 羽を大きく描き、初期案と同じ見た目になる。複数持つ session だけが複数羽になる。

```text
（モック。実装では幅が決定的な ASCII を使う）

        session-auth  3
       2 run · 1 wait
    /)/)    /)/)    /)/)
   ( o.o)  ( o.o)  ( -.-)?
    / > <   / > <  c(")(")
 --v-------v-----------v-----
```

状態ラベル（`2 run · 1 wait`）は色に依存せず内訳を読めるようにするためのもので、省かない。

### 並び順と表示上限

区画の幅は固定なので、横に並べられるうさぎの数には上限がある。上限を超えた分は区画に `+N` と畳む。

- 並び順は **注目度（`Waiting` を先頭）→ stable な agent identity** の順で決める。phase が同じうさぎが
  frame ごとに入れ替わると追えなくなるため、tie-break には runtime の stable ID を使う。
- 畳むのは末尾（注目度の低いほう）からとする。表示枠は `Waiting` が先に使い、`Waiting` 自体が上限を超える
  場合は隠れた `Waiting` の羽数を状態ラベルへ明示する。

### 動きの量

同時に上下へ動くのは `Running` のうさぎだけとし、`Waiting` / idle の耳と瞬きは既存 mascot と同程度の低頻度に
する。agent 単位にすると跳ねるうさぎが増えうるため、既存の stable ID 由来の phase offset を効かせて、
全羽が揃って跳ねないようにする。

### この案で決めていないこと

- **workspace root の agent**。runtime は session に属さないもの（root 実行）を表現できる。庭は session の
  区画しか持たないため、root の agent をどこに描くか、あるいは描かないかは決めていない。
- **agent ごとの表示名**。現在の runtime 参照は表示用の label を持たないため、うさぎは名前ではなく状態と
  位置で区別する。名前を出すなら projection を広げる判断が要る。

## 決定的な配置

うさぎの位置をランダムにすると refresh のたびに session が移動して追いにくい。配置は次の純粋関数で決める。

1. 描画可能領域を、nameplate と表示上限ぶんのうさぎが収まる固定幅の plot に分割する。plot の大きさは
   agent の数で変えない（区画ごとに幅が変わると grid の決定性と hit test が崩れるため）。
2. controller が持つ session 順を plot へ上から下、次に右の列へ割り当てる。
3. 各うさぎの stable `AgentRuntimeId` の先頭 bytes を animation phase の offset にだけ使い、全羽が同時に
   跳ねないようにする。
4. `tick`、projection、領域サイズが同じなら、常に同じ frame を返す。

session が表示可能な列数を超える場合は横方向の viewport とし、`← Scroll` / `Scroll →` button と `←` / `→` key で
1 plot 列ずつ移動する。前後の viewport は端の列を共有するため、現在位置を見失わずに全 session へ到達できる。
resize 後の再配置は許すが、同じ幅・scroll offset の refresh では場所を変えない。

## 起こし方とクリック遷移

入力ごとの挙動は実装済みで、[3. TUI#起こし方とクリック遷移](../03-tui.md#起こし方とクリック遷移) が正本である。
設計判断は次の 3 点にある。

- **最初の入力は wake-up として消費する**。`Ctrl-C` / `Ctrl-Q` も最初の 1 回は終了操作にしない。見えていない
  terminal や modal へ意図しない入力を通さないためである。
- **hitbox は renderer と同じ layout 関数が返す rectangle を使う**。区画に `SessionId`、うさぎ 1 羽に
  `AgentRuntimeId` を束縛し、controller が画面座標から session 順や羽の順を再計算しないため、CJK label、
  端末 resize、表示上限によって click target がずれない。
- **click の粒度はうさぎ = agent とする**。うさぎを押したら訪問先の Closeup でその agent の tab を選ぶ。
  Garden が増やす target semantics は無く、tab の選択は tab strip の click と同じ stable identity の経路を通る。
  一致する tab が無い場合（押した瞬間に終了した、pane を復元していない）は session の Closeup に留めて、
  位置の近い別 tab を選ばない。session そのものの pose（`creating` / `deleting` / `failed` と PR merge の
  celebration）は agent 1 体に対応しないので、`AgentRuntimeId` を束縛しない。
- **うさぎの click は double click を待たない**。screen saver 上では 1 回で訪問できるほうが速く、誤爆しても
  遷移先は読み取り可能な既存 Closeup にすぎない。

session create / remove、Agent launch などの command は Garden に複製しない。Garden から起こせる action は
stable session への既存 Closeup 遷移だけに一本化する。session が 0 件なら空の庭と
`No sessions in the garden` を表示し、通常の wake-up だけを受ける。

これにより Garden は「見る・選ぶ」面に留まり、かわいさのために destructive action や target semantics を
増やさない。

## 端末サイズと motion preference

- 高さ 14 行未満、または幅 64 桁未満では idle threshold を超えても Garden を開かず、既存 Home を保つ。
  screen saver のために操作可能な一覧を警告画面で覆わない。
- 端末の motion preference を直接取得する標準手段はないため、設定に `Animation: full | reduced` は追加しない。
  composition は起動時に `USAGI_REDUCE_MOTION=1` を読み、projection に boolean として注入する。reduced motion
  では全 pose を静止姿勢に固定し、状態ラベルだけ更新する。
- animation は既存 frame tick を共有する。Garden 専用 timer / thread は作らない。
- material が同じ pose の tick は既存 mascot と同様に canonical tick へ畳み、不要な再描画を起こさない。

## presentation 境界

compact renderer と共通 projection は `crates/tui/src/presentation/widgets/garden.rs`、spacious world の純粋 renderer・
生活 cycle・動的 hitbox layout は `crates/tui/src/presentation/widgets/garden_world.rs` に置く。idle clock は
interactive frame loop が monotonic time と user input を観測し、経過時間を注入済みの event として controller へ
渡す。controller 自身は `Instant::now()` を呼ばない（sidebar の double-click 判定が既に取っている形と同じで、
shell が `Instant` を `Duration` へ落として渡す）。overlay lifetime と stable target の検証は
`usecase/application/controller.rs`、Home frame への合成と click の hitbox 解決は
`presentation/views/workspace.rs` が担当する。click 解決が frame と同じ layout を使うのは、描画と hit test が
同じ 1 つの関数呼び出しを共有しているためである。

```text
daemon lifecycle / Agent phase
              │ existing projection
              ▼
       GardenSession[] ── stable SessionId ──► existing Selection / Target
              │
              ▼
 pure garden renderer(tick, size, reduced_motion) ──► hitboxes(SessionId, AgentRuntimeId?, rect)
```

`GardenSession` は表示に必要な `id`、safe label、lifecycle、**その session に属する agent ごとの phase**、
optional な dispatch status、safe failure summary だけを持つ。agent の phase は stable な runtime identity と対で持ち、
並び順の tie-break と、dispatch が `running` の間のうさぎ 1 羽ぶんの hitbox に使う。非 running の dispatch status は
区画全体の静止 pose であり、個別 agent の identity へ誤って束縛しない。
filesystem path、provider-native ID、terminal output、raw error は renderer に渡さない。

## UI sample

純粋 renderer と固定データを使う sample は、次の場面を標準出力へ描く。

| 場面 | 確認できること |
|---|---|
| 120×24 · spacious world 左右端 | notification panel と、巣穴・池・餌場・木陰、左右へ移動するうさぎ、16 cell 単位の camera pan |
| 120×24 · spacious world reduced motion | 全 pose と位置が静止姿勢に固定される |
| 120×24 · session 0 件 | 空の庭と `No sessions in the garden` |
| 120×24 · 2 open projects | project をまたぐ home と観測済み Agent |
| 64×14 terminal の Garden 本体 13 行（左右端） | compact plot への縮退と1列ずつの横スクロール |

```bash
cargo run -p usagi-tui --example garden_sample
```

sample は idle timer、click dispatch には接続しない。生活 cycle、複数 home、compact fallback、端末幅、色と文言を
production 配線より先に確認するための presentation-only surface である。

実際の workspace で見るには、Overview の `garden` command で手動で開くか、5 分間操作せずに待つ（仕様は
[3. TUI#session garden](../03-tui.md#session-garden)）。production の庭は session ごとに controller の runtime-local
phase と最新 coherent Agent inventory を結合して各 agent の phase を描く。controller が runtime の `Ended` /
`Exited` / `Interrupted` を `TargetPhase::Done` へ畳んだ場合も、庭では瞬きへ戻さず静止した `done` pose になる。

**うさぎが居るかどうかは inventory が決める**。controller の runtime-local phase は session が生きている限り
積み上がるので、それを membership に使うと利用者が閉じた agent が `done` のうさぎとして残り、Closeup に
tab が無いうさぎを押せてしまう。tab strip と同じ observation を権威にすることで、庭の羽数と開ける tab が
一致する（正本は [3. TUI#区画とうさぎ](../03-tui.md#区画とうさぎ)）。

## inactive project のうさぎを daemon から観測する

複数 project を開いたときの Garden は、当初 active project にしかうさぎを描かなかった。inactive project は
session / lifecycle の cache だけを持ち、Agent membership は「観測していないので描かない」としていたためである。
これは安全側の判断としては正しいが、Garden の目的（実行状態を一覧表より速く把握する）を開いている project の
数だけ薄めてしまう。庭の半分が常に空区画なら、庭を見る理由が無い。

観測を足す方法は 2 つあった。

| 案 | 内容 | 採否 |
|---|---|---|
| inactive controller を resident にする | project ごとに workspace controller と lane 一式を常駐させる | **不採用**。tab の数だけ terminal 購読・pane 復元・PR 観測が増え、Garden という screen saver のために process の常時コストを倍以上にする |
| workspace を名指しした read-only 観測 | Garden が前面の間だけ `AgentWorkspaceObservation { workspace }` を project ごとに読む | **採用** |

採用案が成立するのは、`AgentWorkspaceObservation` が **connection の bound tenant ではなく request が名指しした
`WorkspaceId`** を daemon 全体の Agent record から filter して答えるからである。daemon は開いている project を
tenant として保持するので、既存の client から他 project の membership をそのまま読める。
`AgentWorkspaceObservation` は runtime inventory と dispatch 由来の session status を一つの read-only response に束ね、
daemon 側の durable record は増やさない。

観測を Garden の表示中に限るのは、他の面が他 project の Agent を描かないからである。閉じた Garden の裏で
読み続ける daemon traffic は誰も見ない。cold start もしない: 観測 lane が daemon を起こせるようにすると、
screen saver が bootstrap lock と lifecycle subprocess を握ることになる。daemon が居なければ、その project は
`project inactive` のままでよい。

**lifecycle は cache のままにする**。inventory は Agent の membership であって session の一覧ではないので、
これを lifecycle の live 性の証拠として使うと、cache が `creating` のまま止まった区画を「今まさに作成中」の
姿で animation させてしまう。そこで `Available` の cached lifecycle だけをうさぎの土台にし、遷移中・失敗の
cached lifecycle は従来どおり静止した `cached · …` に留める（正本は
[3. TUI#inactive project の Agent 観測](../03-tui.md#inactive-project-の-agent-観測)）。

## 実装履歴と受け入れ条件

1. 固定 snapshot から ANSI-safe / width-safe な Garden frame と hitbox を返す widget / unit test を追加する。
2. Garden overlay、idle event、wake / single-click transition を reducer に追加し、注入した経過時間で controller test を
   固定する。
3. interactive frame loop の user activity 観測と Home projection を接続し、screen graph test で自動表示、
   入力消費、overlay 復帰を固定する。
4. `document/03-tui.md` に実装済みの入力・縮退・状態対応を移し、本提案は設計判断だけに縮約する。
5. [うさぎは agent、区画は session](#うさぎは-agent区画は-session) の描画素材へ移し、agent ごとの phase、
   並び順、表示上限、`+N` の畳み込みを追加する。

6. lifecycle 別 animation（`Waiting` の耳交互表示、`Creating` の 2 pose 出現、`Deleting` の段階的 dim）を追加する。
7. `USAGI_REDUCE_MOTION` を composition で読み、renderer が既に受け取る boolean へ配線する。
8. `Failed` の safe failure summary を追加する。session の選択状態は Garden では装飾しない。
9. 複数 project の session を tab 順に束ね、容量超過は Garden 内の横スクロールで全件へ到達可能にする。

10. inactive project の Agent membership を Garden 表示中だけ daemon から観測し、cached lifecycle が
    `Available` の区画へうさぎを描く。
11. うさぎの plot を左領域へ寄せ、右の notification panel に同じ safe projection から導出した現在状態を表示する。
12. 左の Garden 領域が 80×18 以上では session の固定 plot を home の巣穴へ変え、stable identity と tick で再現できる生活 cycle、
    池・餌場・木陰、16 cell 単位の camera pan、移動位置に追随する hitbox を追加する。小さい端末は compact plot を保つ。
13. active / inactive project の dispatch status を同じ deterministic な順位で集約し、非 running status は
    animation と個別 Agent hitbox を持たない session-level の静止 pose にする。

1〜13 はすべて実装済みで、うさぎは agent 単位、非 running の dispatch pose は session 単位である。

受け入れ条件は次のとおりである。

- 同じ入力 snapshot / tick / size は byte-for-byte 同じ frame になる。
- すべての行が端末幅以内で、CJK の session label も途中で壊れない。
- 0 / 1 / 表示上限超過の session、全 lifecycle、narrow / short terminal をテストし、表示上限超過時は
  すべての session と横スクロール button が到達可能である。
- 1 session に複数 agent があるとき、羽数と各 agent の phase が描かれ、集約によって実行中の agent が
  休んでいる姿に化けない。
- 表示上限を超えた agent は `+N` に畳まれる。表示枠は `Waiting` が先に使い、`Waiting` 自体が上限を超える
  場合は隠れた `Waiting` の羽数が明示される。
- agent の並びは phase と stable な runtime identity だけで決まり、同じ素材の frame では入れ替わらない。
- spacious world のうさぎは左右へ歩き、cycle 内で池の飲水・餌場の食事・木陰の睡眠をすべて行う。各時点の
  click rectangle は描いた sprite の viewport 座標と一致し、camera を最後まで動かすと全 session の巣穴へ到達できる。
- animation の pose が変わらない tick では frame material も変わらない。
- 5 分未満では Garden を開かず、5 分到達時に eligible な Home だけで開く。
- backend event と terminal output は idle deadline を延長せず、user input と resize は延長する。
- wake-up の最初の入力は背面へ伝播せず、うさぎ click だけが対応する既存 Closeup へ遷移する。
- Garden から daemon command を直接発行しない。observation lane は read-only で、daemon を起動しない。
- 開いているどの project の session も、その project の Agent を観測できていればうさぎになり、観測できて
  いなければ推測されない。
- notification panel は表示中の plot と同じ viewport を説明し、完了、入力待ち、実行中、失敗を安全な文で区別する。
- runtime record の順序を入れ替えても dispatch status の集約は変わらず、非 running status の区画は tick が進んでも
  静止し、個別 Agent hitbox を持たない。
- selected session が snapshot 更新で消えた場合は、既存 reconciliation と同じ surviving session へ着地する。

## 採用しない案

- **session row の左に小さなうさぎを並べる**: 一覧密度を落とす割に「庭」の空間表現にならない。
- **常に右ペインを Garden にする**: Switch の cursor preview と live pane の視認性を失う。無操作時だけ全幅表示する。
- **Garden 上で通常キーをそのまま実行する**: 見えていない terminal や modal に意図しない入力が入るため、最初の
  入力は wake-up として消費する。
- **非決定的な連続物理 simulation で歩かせる**: frame の決定性、hit test、テスト、低負荷 redraw と相性が悪い。
  実装した spacious world は stable runtime identity と注入 tick から位置を求める固定 cycle とし、自由な移動表現を
  入れながら同じ material の再現性を保つ。
- **状態を色だけで表す**: 端末テーマと色覚差に依存するため、太字・顔・text label を必ず併用する。
