---
number: 700
title: fix(daemon): supervised daemon の起動 directory を pin し、unit の妥当性を systemd に検証させる
status: done
priority: high
labels: [daemon, core]
dependson: []
related: [697, 699]
created_at: 2026-08-19T10:57:59.255418+00:00
updated_at: 2026-08-19T10:57:59.255418+00:00
---

## 概要

[#699](699-feat-daemon-linux-systemd-user-unit-service-macos.md) の self review で見つけた欠陥をまとめて直す。
中心は **`usagi daemon install-service` で登録した daemon が新規インストールで起動できない**ことである。

## 欠陥 1（blocking）: 起動 directory が pin されていない

daemon が束ねる workspace は**起動時 directory**から決まる（`sessions.json` に durable な `repository_root` が
あればそれが勝つ）。supervisor は install した shell の working directory を引き継がず、既定は

- systemd **user** unit → 利用者の home
- launchd → `/`

のいずれも**利用者が選んだ workspace ではない**。

さらに悪いのは、workspace root が home directory になると、workspace fence
（`<workspace>/.usagi/daemon/daemon.lock`）と単一インスタンス lock（`<data-dir>/daemon/daemon.lock`）が
既定の `~/.usagi` data home のもとで**同一ファイル**を指すことである。daemon は前者を取ってから後者を取れず、
**自分の起動を「daemon already running」として拒否し続ける**。

### 実測（release binary、production 既定、隔離した HOME）

```
$ cd $HOME && usagi daemon serve
usagi v2.6.0: daemon already running        ← 他に daemon は居ない。誤報
作られたもの: .usagi/daemon/{daemon.lock, record.lock}   ← ここで停止

$ cd <HOME 外> && usagi daemon serve         ← cwd だけ変えた対照実験
（fence 取得 → instance lock 取得 → socket bind まで到達）
作られたもの: ws/.usagi/daemon/daemon.lock
            fh3/.usagi/daemon/{daemon.lock, record.lock, current.lock, generations}
```

client からの cold start は `lifecycle_command` が同じ罠を避けるため cwd を pin しており
（"opening `~/project` from `~` would cold-start a daemon bound to `~`" とコメントがある）、
**supervised start だけが同じ保護を受けていなかった**。

### 対応

`install_service` が `bound_workspace_root` で workspace root を解決し、定義へ書き込む。

| supervisor | 追加した field |
|---|---|
| systemd | `WorkingDirectory=<workspace>` |
| launchd | `<key>WorkingDirectory</key>` |

workspace root が UTF-8 で綴れない場合は、base と同じく lossy 変換せず install を拒否する。

## 欠陥 2: `Environment=` の quote 形式が `systemd.exec(5)` の記載と違う

`Environment=USAGI_HOME="/path"`（値だけを quote）で出していた。man の記載は
`Environment="VAR=word1 word2"` と **`NAME=value` 全体**を quote する形である。parser が mid-word の quote を
受理する可能性は高いが、依存する理由がない。`assignment()` を足して記載どおりの形へ変えた。

## 欠陥 3: unit が systemd に受理されるかを誰も検証していない

既存 test はすべて**自分が生成した文字列との比較**なので、systemd が拒否する directive や quote 形式でも
全部 green になる（欠陥 2 がまさにそれで、形式を変えたら 4 件が落ちて初めて気づいた）。

`systemd-analyze verify` に実際の unit を渡す test を追加した（Linux のみ。`systemd-analyze` が無い環境では
skip する）。`ExecStart` / `WorkingDirectory` は verify が実体を解決するため、存在する path を使う。

## 欠陥 4: systemd の最低バージョン要件が未記載

`StandardError=append:` は **systemd 240 以降**でしか使えず、それ以前では unit が load に失敗する。
docs（`05-daemon.md` の supervisor 表、README）に明記した。

## 欠陥 5（nit）: `allow(dead_code)` が file 全体

rustc の liveness は伝播するため、entry point だけに付けても呼び先が dead root のまま残り、item 単位に
するには 10 個の attribute が必要になる。**範囲を狭める代わりに、検出漏れが起きる条件をコメントで明示**した。

この allowance が有効になるのは real IO が cfg で外れる側だけなので:

- `systemd.rs` → macOS build で有効（CI は Linux なので **CI が dead code を検出する**）
- `launchd.rs` → Linux build で有効（CI では検出されない。**macOS build が検出する**）

## テスト

- `cargo test -p usagi --bin usagi runtime::systemd`: 12 件（`WorkingDirectory` の pin、quote / specifier escape、
  非 UTF-8 workspace の拒否、`systemd-analyze verify`、既存の 8 件）
- `cargo test -p usagi --bin usagi runtime::launchd`: 8 件（`WorkingDirectory` の pin と XML escape、
  非 UTF-8 workspace の拒否を追加）
- `cargo llvm-cov --summary-only --bin usagi`: `systemd.rs` / `launchd.rs` ともに Functions・Lines 100%
- `cargo clippy --workspace --all-targets -- -D warnings` を **macOS と `x86_64-unknown-linux-gnu` の両ターゲット**で実行
  （#699 では macOS だけで検証したため Linux のコンパイル不能を CI まで見落とした）
