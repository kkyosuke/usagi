# 15. session garden

> [設計提案一覧](README.md) ｜ 関連仕様: [TUI](../03-tui.md) ｜ 実装 issue: #674

session を庭にいるうさぎとして表す、Home の screen saver UI を提案する。一定時間操作がなければ Garden が
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

## 状態と動き

描画は現在の `ProjectedSession.lifecycle` と controller が集約済みの Agent phase だけから導出する。見た目の
ために daemon schema、永続 session record、IPC event を増やさない。

| projection | 庭での表現 | animation |
|---|---|---|
| `Available` + Agent `Running` | 前へ跳ねる | 低い姿勢 → 空中 → 着地の 3 pose |
| `Available` + Agent `Waiting` | 座って首をかしげる | `?` と耳をゆっくり交互表示 |
| `Available` + Agent `Ready` / Agent 無し | 草のそばで休む | ときどき瞬きする |
| `Creating` | 土から耳だけ見える | 2 pose の出現 animation |
| `Failed` | 伏せて止まる | animation しない。短い safe failure label を添える |
| `Deleting` | 奥へ帰る | 位置は固定し、段階的に dim にする |

motion は状態の意味を補助するだけにし、状態ラベルを省かない。画面全体が忙しくならないよう、同時に上下へ
動くのは `Running` のうさぎだけとする。`Waiting` / idle の耳と瞬きは既存 mascot と同程度の低頻度にする。

## 決定的な配置

うさぎの位置をランダムにすると refresh のたびに session が移動して追いにくい。配置は次の純粋関数で決める。

1. 描画可能領域を、うさぎ 1 羽と nameplate が収まる固定幅の plot に分割する。
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

`GardenSession` は表示に必要な `id`、safe label、lifecycle、Agent phase、safe failure summary だけを持つ。
filesystem path、provider-native ID、terminal output、raw error は renderer に渡さない。

## UI sample

純粋 renderer と固定データを使う sample は、100×24 の Garden を標準出力へ描く。

```bash
cargo run -p usagi-tui --example garden_sample
```

sample は idle timer、Home overlay、click dispatch には接続しない。状態別 pose、複数 plot、端末幅、色と文言を
production 配線より先に確認するための presentation-only surface である。

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
