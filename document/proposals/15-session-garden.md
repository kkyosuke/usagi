# 15. session garden

> [設計提案一覧](README.md) ｜ 関連仕様: [TUI](../03-tui.md) ｜ 実装 issue: #674

session を庭の区画、その agent を区画にいるうさぎとして表す、Home の screen saver UI を提案する。一定時間操作がなければ Garden が
自動的に現れ、入力すると元の画面へ戻る。目的は session 数や実行状態を一覧表より速く把握できることと、
`usagi` らしさを操作の邪魔にならない範囲で強めることである。

Garden は session の lifecycle や Agent phase を所有しない。daemon-authoritative な既存 projection を
絵へ写すだけで、session の選択と操作は引き続き stable `SessionId` を使う。

## 画面案と遷移

Garden は Home の一時的な全幅レイヤーとして開く。常駐 route の Switch / Closeup は増やさず、背面の route、
active session、pane、terminal subscription を変えない。Garden を閉じると、表示前と同じ Home へ戻る。

既定の idle threshold は **5 分**とする。キー、paste、mouse button、wheel、terminal resize のいずれかを受けるたびに
monotonic clock 上の最終操作時刻を更新する。tick、daemon/backend event、Agent/terminal 出力は利用者の操作ではないため
idle timer を延長しない。これにより Agent が動き続けていても、人が操作していなければ Garden を表示できる。

ただし、確認 modal、編集中の form、Director drawer が前面にある間は自動表示しない。未送信入力や destructive action の
確認を Garden で覆わないためである。通常の Switch と、overlay のない Closeup（live terminal を含む）は自動表示の対象とし、
daemon-owned process は背面で動き続ける。

```text
 usagi / my-project                                      3 sessions · 1 running
 ──────────────────────────────────────────────────────────────────────────────

       .  *                    session-auth
              (\(\                 running
           ＿( o.o)    ぴょん
             > ^ <

                 🌱                         issue-647
       ──v────v──────────────        >    (\(\        waiting
                                            (o.o)?
                            coder          o(_(")(")
            (\(\          available
            ( -.-)
           o(_(")(")             ·  ·  ·                  failed-build
                                                          ×(x.x)
       ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~
       Garden  click a usagi to visit · any key to return
```

絵文字は端末で 1 桁または 2 桁になり得るため、production の地面・草花は ASCII を基本にする。上の `🌱` は
雰囲気を示すモックであり、実装では幅が決定的な `*` / `.` / `v` を使う。うさぎ本体は既存 mascot と同じ
`Role::Feature`、選択中の nameplate と `>` は `Role::Accent`、失敗は `Role::Danger` で描く。色を見分けられない
場合にも `>`、状態ラベル、顔の違いで判別できる。

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

見た目のために daemon schema、永続 session record、IPC event は増やさない。agent ごとの phase は
controller が既に持っている runtime 一覧から導出する。

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
- 畳むのは末尾（注目度の低いほう）からとする。人の入力を待っている agent は必ず見える。

### 動きの量

同時に上下へ動くのは `Running` のうさぎだけとし、`Waiting` / idle の耳と瞬きは既存 mascot と同程度の低頻度に
する。agent 単位にすると跳ねるうさぎが増えうるため、既存の stable ID 由来の phase offset を効かせて、
全羽が揃って跳ねないようにする。

### この案で決めていないこと

- **click の粒度**。現在の hitbox は区画 = session で、遷移先も session の Closeup に一本化している
  （[起こし方とクリック遷移](#起こし方とクリック遷移)）。うさぎ単位の hitbox にして「その agent の tab へ入る」
  ことは自然な拡張だが、遷移先が増えるため別途決める。
- **workspace root の agent**。runtime は session に属さないもの（root 実行）を表現できる。庭は session の
  区画しか持たないため、root の agent をどこに描くか、あるいは描かないかは決めていない。
- **agent ごとの表示名**。現在の runtime 参照は表示用の label を持たないため、うさぎは名前ではなく状態と
  位置で区別する。名前を出すなら projection を広げる判断が要る。

## 決定的な配置

うさぎの位置をランダムにすると refresh のたびに session が移動して追いにくい。配置は次の純粋関数で決める。

1. 描画可能領域を、nameplate と表示上限ぶんのうさぎが収まる固定幅の plot に分割する。plot の大きさは
   agent の数で変えない（区画ごとに幅が変わると grid の決定性と hit test が崩れるため）。
2. controller が持つ session 順を plot へ左上から割り当てる。
3. stable `SessionId` の先頭 bytes を animation phase の offset にだけ使い、全羽が同時に跳ねないようにする。
4. `tick`、projection、領域サイズが同じなら、常に同じ frame を返す。

session が plot 数を超える場合は末尾を `+ N more in session list` に畳み、既存 sidebar を完全な一覧の正本として
残す。resize 後の再配置は許すが、同じ幅の refresh では場所を変えない。

## 起こし方とクリック遷移

- **うさぎを single click**: その plot に束縛した stable `SessionId` を active / selected にして Garden を閉じ、
  既存 Closeup へ入る。double click 待ちは入れず、screen saver 上では 1 回で訪問できる。
- **うさぎ以外を click**: click を消費して Garden を閉じ、表示前の Home へ戻る。
- **任意の key / paste / wheel**: 最初の入力を wake-up として消費し、表示前の Home へ戻る。入力を背面の terminal や
  form へ渡さない。`Ctrl-C` / `Ctrl-Q` も最初の 1 回は終了操作にせず Garden を閉じる。
- **terminal resize**: Garden を閉じて idle timer を reset する。
- **session が 0 件の Garden**: 空の庭と `No sessions in the garden` を表示し、通常の wake-up だけを受ける。

plot の hitbox は renderer と同じ layout 関数が返す `SessionId` 付き rectangle を使う。controller が画面座標から
session 順を再計算しないため、CJK label、端末 resize、表示上限によって click target がずれない。click と同時に
snapshot から session が消えていた場合は stale target を実行せず、Garden を閉じるだけにする。

session create / remove、Agent launch などの command は Garden に複製しない。Garden から起こせる action は
stable session への既存 Closeup 遷移だけに一本化する。

これにより Garden は「見る・選ぶ」面に留まり、かわいさのために destructive action や target semantics を
増やさない。

## 端末サイズと motion preference

- 高さ 14 行未満、または幅 64 桁未満では idle threshold を超えても Garden を開かず、既存 Home を保つ。
  screen saver のために操作可能な一覧を警告画面で覆わない。
- 端末の motion preference を直接取得する標準手段はないため、初期実装は設定に `Animation: full | reduced` を
  追加しない。まず `USAGI_REDUCE_MOTION=1` を composition で読み、projection に boolean として注入する案を
  検証する。reduced motion では全 pose を静止姿勢に固定し、状態ラベルだけ更新する。
- animation は既存 frame tick を共有する。Garden 専用 timer / thread は作らない。
- material が同じ pose の tick は既存 mascot と同様に canonical tick へ畳み、不要な再描画を起こさない。

## presentation 境界

実装時は `crates/tui/src/presentation/widgets/garden.rs` に純粋 renderer と hitbox layout を置く。idle clock は
interactive frame loop が monotonic time と user input を観測し、controller へ `IdleElapsed` / `WakeGarden` のような
時刻を注入済みの event を渡す。controller 自身は `Instant::now()` を呼ばない。overlay lifetime と stable target の
検証は `usecase/application/controller.rs`、Home frame への合成は `presentation/views/workspace.rs` が担当する。

```text
daemon lifecycle / Agent phase
              │ existing projection
              ▼
       GardenSession[] ── stable SessionId ──► existing Selection / Target
              │
              ▼
 pure garden renderer(tick, size, reduced_motion) ──► hitboxes(SessionId, rect)
```

`GardenSession` は表示に必要な `id`、safe label、lifecycle、**その session に属する agent ごとの phase**、
safe failure summary だけを持つ。agent の phase は stable な runtime identity と対で持ち、並び順の tie-break に使う。
filesystem path、provider-native ID、terminal output、raw error は renderer に渡さない。

## UI sample

純粋 renderer と固定データを使う sample は、次の 4 場面を標準出力へ描く。

| 場面 | 確認できること |
|---|---|
| 100×24 · 全 lifecycle | 状態別 pose・状態ラベル・3 列 2 行の plot |
| 100×24 · reduced motion | 全 pose が静止姿勢に固定される |
| 100×24 · session 0 件 | 空の庭と `No sessions in the garden` |
| 64×14 · 最小サイズ | 2 列 1 行への縮退と `+ N more in session list` |

```bash
cargo run -p usagi-tui --example garden_sample
```

sample は idle timer、click dispatch には接続しない。状態別 pose、複数 plot、端末幅、色と文言を
production 配線より先に確認するための presentation-only surface である。

実際の workspace で見るには、Overview の `garden` command で手動で開く（仕様は
[3. TUI](../03-tui.md#overview-と-modal)）。自動表示（idle threshold）と click 遷移はまだ接続しておらず、
現時点の Garden は「開く / 眺める / 最初の入力で戻る」だけである。production の庭に出る pose は controller が
集約した phase に従うため、`interrupted` は `Done` へ畳まれて `available` の pose になる。

## 実装順序と受け入れ条件

1. 固定 snapshot から ANSI-safe / width-safe な Garden frame と hitbox を返す widget / unit test を追加する。
2. Garden overlay、idle event、wake / single-click transition を reducer に追加し、monotonic fake clock で controller test を
   固定する。
3. interactive frame loop の user activity 観測と Home projection を接続し、production screen graph test で自動表示、
   入力消費、overlay 復帰を固定する。
4. `document/03-tui.md` に実装済みの入力・縮退・状態対応を移し、本提案は設計判断だけに縮約する。

受け入れ条件は次のとおりである。

- 同じ入力 snapshot / tick / size は byte-for-byte 同じ frame になる。
- すべての行が端末幅以内で、CJK の session label も途中で壊れない。
- 0 / 1 / 表示上限超過の session、全 lifecycle、narrow / short terminal をテストする。
- 1 session に複数 agent があるとき、羽数と各 agent の phase が描かれ、集約によって実行中の agent が
  休んでいる姿に化けない。
- 表示上限を超えた agent は `+N` に畳まれ、`Waiting` の agent は畳まれずに必ず見える。
- agent の並びは phase と stable な runtime identity だけで決まり、同じ素材の frame では入れ替わらない。
- animation の pose が変わらない tick では frame material も変わらない。
- 5 分未満では Garden を開かず、5 分到達時に eligible な Home だけで開く。
- backend event と terminal output は idle deadline を延長せず、user input と resize は延長する。
- wake-up の最初の入力は背面へ伝播せず、うさぎ click だけが対応する既存 Closeup へ遷移する。
- Garden から daemon command を直接発行しない。
- selected session が snapshot 更新で消えた場合は、既存 reconciliation と同じ surviving session へ着地する。

## 採用しない案

- **session row の左に小さなうさぎを並べる**: 一覧密度を落とす割に「庭」の空間表現にならない。
- **常に右ペインを Garden にする**: Switch の cursor preview と live pane の視認性を失う。無操作時だけ全幅表示する。
- **Garden 上で通常キーをそのまま実行する**: 見えていない terminal や modal に意図しない入力が入るため、最初の
  入力は wake-up として消費する。
- **物理 simulation で自由に歩かせる**: frame の決定性、hit test、テスト、低負荷 redraw と相性が悪い。
- **状態を色だけで表す**: 端末テーマと色覚差に依存するため、顔・marker・text label を必ず併用する。
