# 5.1 モードとモーダル

> [ホーム画面トップ](README.md) ｜ [画面設計トップ](../README.md) ｜ 次へ → [5.2 レイアウトと各モードの表示](02-layout.md)

ホーム画面のトップレベル mode は **Switch** と **Closeup** の 2 つだけです。`Overview` は常駐 mode ではなく、必要なときに重なる Workspace スコープのモーダル surface です。Closeup mode で開く Session スコープのアクション surface は **Closeup モーダル**（タイトル `Closeup: <name>`）として扱います。

## 目次

- [2 つの mode](#2-つの-mode)
- [2 つの modal](#2-つの-modal)
- [状態遷移](#状態遷移)
- [コマンドスコープ](#コマンドスコープ)

## 2 つの mode

| mode | 操作対象 | キー入力の主な行き先 | 右ペイン |
|---|---|---|---|
| **Switch** | セッション群の操作 | 左ペイン | 選択中セッションを開いたときのプレビュー。ライブならタブ＋端末 snapshot、非ライブなら休止表示 |
| **Closeup** | 選択中セッションの中の操作 | Closeup モーダル／タブ／ライブ端末 | Closeup モーダル、ペイン preview、またはライブ埋め込み端末 |

Switch は起動直後の既定 mode です。Closeup は、選択中セッションで実行するアクションとライブ端末をまとめる mode です。ライブ端末にアタッチしている状態は Closeup の内部状態であり、mode ladder には `Closeup live` を出しません。

## 2 つの modal

| modal | 開き方 | スコープ | 内容 |
|---|---|---|---|
| **Overview** | `:` | Workspace | `session` / `unite` / `issue` / `config` / `env` など、ワークスペース全体のコマンド入力 |
| **Closeup** | `Ctrl-O a`（ライブ端末では `Alt-a` も） / `t` / 非ライブセッションの確定 | Session | `terminal` / `agent` / `close` / `diff` など、選択中セッションのアクション UI（Menu / Prompt） |

`Overview` は「全体を見る」ためのモーダル、`Closeup` モーダルは「このセッションで何をするか」を選ぶモーダルです。Closeup という名前はトップレベル mode とモーダルタイトルの両方に使いますが、後者は Closeup mode 内で浮く Session スコープのアクション surface を指します。

## 状態遷移

```text
起動 → Switch

Switch --Enter（ライブ）------------------------------▶ Closeup（ライブ端末）
Switch --Enter（非ライブ）/ t / Ctrl-O a--------------▶ Closeup（Closeup modal）
Switch --:--------------------------------------------▶ Overview modal
Switch --Esc------------------------------------------▶ 無効

Closeup（Closeup modal）--terminal/agent----------------▶ Closeup（ライブ端末）
Closeup（ライブ端末）--Ctrl-O a / Alt-a---------------▶ Closeup modal（同じ Closeup 内）
Closeup（ライブ端末）--Ctrl-O o / Alt-o---------------▶ Switch
Closeup（Closeup modal / preview）--Ctrl-O o / Esc------▶ Switch
Closeup --:-------------------------------------------▶ Overview modal
```

`Ctrl-O a` は Closeup モーダルを開きます。Switch で押すと選択中セッションの Closeup モーダルを開き、Closeup のペイン preview 上で押すとその preview の上に Closeup モーダルを再表示します。ライブ端末では同じキーが端末 driver から Closeup に戻る操作として扱われます。

`:` は Overview モーダルを開きます。Overview モーダルは Workspace スコープの入力面であり、`Esc` で閉じると元の mode に戻ります。

## コマンドスコープ

| スコープ | 入力面 | 主なコマンド |
|---|---|---|
| **Workspace** | Overview モーダル（`:`） | `session` / `unite` / `issue` / `config` / `env` / `doctor` / `man` |
| **Session** | Closeup モーダル（Closeup） | `terminal` / `agent` / `close` / `diff` |
| **Both** | 両方 | `clear` など |

現在のスコープはフッターに表示します。Switch のフッターは `[switch]`、Closeup の Closeup モーダルは `[session: <name>]`、Overview モーダルは `[command]` として表示します。
