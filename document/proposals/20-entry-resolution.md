# 20. 起動時 workspace 解決と entry 面の回収

> [設計提案一覧](README.md) ｜ 関連する現在仕様: [3. TUI](../03-tui.md#画面と入力) / [1. プロジェクト概要](../01-overview.md#cli) ｜ 関連提案: [17-multi-workspace-daemon.md](17-multi-workspace-daemon.md)

> **Status:** 提案中（未実装）。本書は Welcome の Open / New / Recent を Home の既存面と起動時解決へ畳み込む target design である。
> 現在のビルドの挙動は [3. TUI](../03-tui.md) と [11. キーバインド](../11-keybindings.md) を正本とする。
>
> **Baseline:** 原版 commit `573dc395df99270f53baaca1bcc4b8d5861b0823`（2026-09-15）。現在仕様は上記の TUI とキーバインドを参照する。

## 目的

Welcome の Open / New / Recent が日常的に使われていない。この 3 つを「削る / 残す」の二択で扱わず、
**それぞれが答えている問いを、より早い場所へ移して回収する**ための設計判断を残す。

回収先は次の 2 つで、どちらも既に存在する。

- **起動時の解決**: 「どの workspace か」は cwd と直近の利用履歴でほぼ決まっており、起動のたびに人へ問い直す必要がない。
- **Home の workspace deck**: `+ Open` overlay と project tab は、Welcome の Open / New（Existing）と同じ操作を
  作業中の文脈のまま提供する（[project tab と workspace deck](../03-tui.md#project-tab-と-workspace-deck)）。

## 問題の構造

Welcome が使われない理由は「画面の出来」ではなく、**位置**と**重複**である。

1. **問いのタイミングが早すぎる**。`usagi` は引数なしで必ず Welcome を出す（[process argv contract](../02-architecture.md#process-argv-contract)）。
   しかし起動時点の答えは、ほとんどの場合 cwd の repository か、前回と同じ deck である。既に決まっている問いに
   毎回 1 画面と数打鍵を払っている。
2. **Home に同じ操作がある**。workspace を開いたあとは deck から離れずに追加・切り替え・登録ができるため、
   Welcome へ戻る動機が無い。
3. **CLI に近道がある**。`usagi open [path]` は登録と Home 起動を 1 コマンドで行い、Welcome を通らない。

### 重複マトリクス

| Welcome の項目 | Home の等価物 | CLI の等価物 | Welcome 固有か |
|---|---|---|---|
| Open（登録済み一覧・filter・Unite 選択） | `+ Open` overlay（filter・`Space` 複数選択・`Ctrl-X` close） | — | 重複 |
| Recent（カード 3 枚） | project tab deck（開いている deck そのもの） | `usagi open` | 重複（起動直後だけ意味を持つ） |
| New / Existing（既存ディレクトリ登録） | `+ Open` overlay の Directory 入力 | `usagi open <path>` | 重複 |
| New / Clone（git clone して登録） | なし | なし | **固有** |
| Config | Overview の `config`（overlay modal） | `usagi config` | 重複 |
| Quit | `Ctrl-Q` の exit prompt | — | 重複 |

**Welcome 固有の機能は Clone だけである**。したがって「Welcome を作り込む」方向に投資しても、
重複が解消しない限り使われ方は変わらない。

## 設計判断

1. **起動時に workspace を解決し、解けたら Welcome を出さない**。Recent は「画面」ではなく「起動の既定値」として回収する。
2. **Open / New を `+ Open` overlay へ一本化する**。overlay に Clone モードを足し、Welcome 固有の機能を残さない。
3. **Welcome は Picker へ縮退させ、消さない**。解決不能時と workspace 離脱時の戻り先という 2 つの役割は残るため、
   2 カラムのメニュー＋カードをやめ、Home の overlay と同じ 1 リストにする。

### 起動時の解決順序

```text
usagi（引数なし）
   |
   +-- cwd が登録済み workspace の内側 ------> その workspace の Home
   |
   +-- 直近の Recent が解決できる ----------> その単体 / Unite deck の Home
   |        （Recent は Unite も保持するため前回の deck 構成をそのまま復元する）
   |
   +-- 登録が 0 件 ------------------------> New（初回導線。ここだけ New が主役になる）
   |
   `-- 解決先を開けない（daemon 拒否 / 消失）--> Picker ＋ notice（無言で別 workspace へ落ちない）
```

- 解決は registry と Recent だけを読み、daemon には問い合わせない（entry 面が daemon 非依存である原則を保つ）。
- cwd 判定は登録済み path の祖先一致で行い、**未登録ディレクトリを暗黙に登録しない**。暗黙登録は `usagi open` の
  明示的な操作に留める。
- 解決結果は `usagi open <path>` と同じ snapshot / composition 経路へ入れる。新しい Home 起動経路は作らない。
- **明示的に Picker を開く入口を 1 つ残す**。現在 Welcome の互換 alias である `usagi hop` をこの用途へ再定義し、
  「今日はどの workspace か選びたい」を 1 コマンドで表す。

### 画面の before / after

| 面 | 現在 | 提案後 |
|---|---|---|
| `usagi` | Welcome（メニュー＋Recent カード） | 解決した workspace の Home。解決不能時だけ Picker |
| `usagi hop` | Welcome（非表示 alias） | Picker（公開コマンド） |
| Welcome の Open | 専用画面 | 廃止。`+ Open` overlay と Picker が担う |
| Welcome の New | 専用画面（Clone / Existing） | 初回導線と `+ Open` overlay の Clone モードへ分解 |
| Welcome の Recent | カード 3 枚 | 起動時解決の第 2 候補。Picker では最近順の並び順として残る |
| `Ctrl-Q` → `w` | Welcome へ戻る | Picker へ戻る（[workspace の離脱と終了](../03-tui.md#workspace-の離脱と終了)の意味は変えない） |

Picker は filter 1 行＋ 1 リスト（最近順 → 名前順）で、footer は `n: new  c: config  q: quit` に縮める。
マスコットと splash は起動の顔として Picker と Home 起動の両方で保つ。

## 段階

各段階は単独で出荷でき、前段が無くても後段の価値を壊さない。

| 段階 | 内容 | 主な変更点 | 効果 |
|---|---|---|---|
| 1 | 起動時解決 | 合成ルートの entry 決定（`src/runtime/tui.rs`）と `EntryScreen` 選択、`usagi hop` の再定義 | 日常の起動から Welcome が消える。Recent / Open / New に触れない |
| 2 | Clone の移設 | `+ Open` overlay に Clone モード、New の Clone 経路を overlay の backend port へ寄せる | Welcome 固有機能が無くなる |
| 3 | Welcome → Picker | `views/welcome.rs` の 2 カラム描画を 1 リスト化、`views/open.rs` と統合、Home の `w` 戻り先を差し替え | 画面が 3 つ（Welcome / Open / New）から 2 つ（Picker / New）へ減る |

段階 1 だけでも「使われない画面を毎回見る」問題は解消する。段階 2 / 3 は重複そのものの除去であり、
実装より先に段階 1 の実使用で「Picker をどれだけ開くか」を観測してから着手してよい。

## 却下した代替案

| 案 | 却下理由 |
|---|---|
| Welcome を即削除し、解決できない場合は空の Home を出す | 0 件 deck の Home は active tab を持てず、`+ Open` overlay だけが載った特殊な Home を新設することになる。画面は減らず、状態が増える |
| Welcome に横断情報（全 workspace の session / Garden）を足して価値を上げる | 起動のたびに「どの workspace か」を問う構造は変わらない。横断表示は既に全幅 Session Garden が deck 上で担っており、entry 面へ複製すると SSoT が割れる |
| 起動挙動を Global 設定にする | 既定の解決順序が正しければ設定は不要で、entry の分岐と設定 surface を同時に増やす。明示操作は `usagi hop` と `usagi open <path>` の 2 つで足りる |
| cwd が未登録なら暗黙に登録して開く | 任意のディレクトリで `usagi` を打つだけで registry が汚れる。登録は `usagi open` の明示操作に留める |

## 未決事項

- Picker と `+ Open` overlay を同じ view 実装で共有するか、描画だけ揃えて別実装のままにするか。
- 段階 1 で解決した workspace が **Unite deck** の場合、Recent の Unite entry をそのまま復元するか、
  active tab だけを開いて残りを遅延で開くか（起動時間とのトレードオフ）。
- 初回導線（登録 0 件）を New 全画面のままにするか、Picker の空状態に Clone / Existing の 2 行を出すだけにするか。
