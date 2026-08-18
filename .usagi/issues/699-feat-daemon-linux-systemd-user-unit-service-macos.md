---
number: 699
title: feat(daemon): Linux に systemd user unit を追加して service 監視を macOS と揃える
status: done
priority: high
labels: [daemon, core]
dependson: []
related: [697]
created_at: 2026-08-18T00:48:37.361271+00:00
updated_at: 2026-08-18T00:48:37.361271+00:00
---

## 概要

`usagi daemon install-service` が **macOS 専用**（LaunchAgent）で、Linux では `Unsupported` で失敗していた。
systemd unit も存在しなかった（リポジトリ全体で `systemd` の参照ゼロ）。

このため Linux では detached `start` と client bootstrap の自動起動だけになり、**ログアウト・再起動をまたいで
生き残らず、異常終了後の自動復帰も無い**状態だった。v2 は Linux amd64 を出荷するため、この非対称を解消する。

## やったこと

`src/runtime/systemd.rs` を追加し、`launchd.rs` と同じ形（pure な planning / rendering ＋ DI した real IO）で
systemd **user** unit を provision する。

```
[Unit]
Description=usagi daemon

[Service]
Type=simple
ExecStart="<exe>" daemon serve
Restart=on-failure
RestartSec=1
Environment=USAGI_HOME="<base>"
Environment=USAGI_RUNTIME_MODE=<mode>
StandardError=append:<selected>/logs/systemd-daemon.stderr.log

[Install]
WantedBy=default.target
```

| 項目 | 決定 | 理由 |
|---|---|---|
| user unit（system unit ではない） | `<config dir>/systemd/user/usagi-daemon.service` | daemon は 1 人の利用者の PTY と Agent child を所有し、その利用者の home 配下に data home を解決する。system unit は root で動き別の data home を解決してしまう |
| unit の置き場所 | `$XDG_CONFIG_HOME`（未設定なら `~/.config`） | `dirs::config_dir()` を注入。pure 側は config dir を引数で受ける |
| `Restart=on-failure`（`always` ではない） | **launchd と意図的に異なる** | LaunchAgent の `KeepAlive` は無条件なので `usagi daemon stop` の後でも起動し直す。`stop` が停止の正規手段である以上、supervision がそれを打ち消すべきではない。graceful な停止は停止のまま残し、crash だけ回復する |
| data home の組を運ぶ | `Environment=` に `USAGI_HOME` と `USAGI_RUNTIME_MODE` | systemd は install した shell の環境ではなく systemd 自身の環境で起動する。組が無いと空の環境から data home を再解決し、log と daemon の mode が食い違う（#697 と同じ欠陥） |
| quote / escape | shell 風に分割される field を quote し `%` を `%%` へ | quote しない path に空白があると誤った argv で起動する。escape しない `%` は unit specifier として展開される |
| 非 UTF-8 path | lossy 変換せず `InvalidInput` で拒否 | 壊れた base を書いた unit は、誰も選んでいない directory へ daemon を送る |
| install 手順 | `daemon-reload` → `enable --now` | reload を先に行わないと、systemd が読んでいない unit を enable することになる |
| uninstall 手順 | `disable --now`（失敗許容）→ file 削除 → `daemon-reload`（失敗許容） | 既に inactive でも file 削除は必須（残すと次の login で再活性化する） |

### platform dispatch

`daemon.rs` に `install_service` / `uninstall_service` を置き、macOS は launchd、Linux は systemd、それ以外は
`Unsupported` を返す。出力の supervisor 名も `SERVICE_SUPERVISOR` 定数で platform ごとに切り替える。

### real IO を platform で閉じた

`launchd.rs` / `systemd.rs` の `mod real_io` と再 export をそれぞれの platform へ `cfg` で閉じ、`launchctl` /
`systemctl` 内の「他 platform では Unsupported」分岐を削除した（dispatch 側が担うので冗長だった）。
pure な planning / rendering は cross-platform のまま保ち、**どのホストでもテストが走る**。非対象 platform では
test からのみ到達するため、file 冒頭に `cfg_attr(not(target_os = ...), allow(dead_code))` を理由付きで置いた。

### coverage

`src/runtime/systemd.rs` の `real_io` を `coverage-off-allowlist.json` へ `reason=real_io` で登録した
（`ruby scripts/coverage-off-lint.rb` → ok, 229 exclusions）。

### docs

- `document/05-daemon.md` の `## launchd supervision` を `## service supervision` へ改題し、2 つの supervisor を
  表で対照した。**異常終了後の扱いが異なる**こと、Linux の `loginctl enable-linger` が別途必要なことも明記した。
- daemon CLI 表の `install-service` / `uninstall-service` の記述を platform 中立へ更新した。
- `README.md` の CLI 表と service に関する注記を更新し、登録しなくても client bootstrap で自動起動することを明記した。

## テスト

- `cargo test -p usagi --bin usagi runtime::systemd`: 10 件（unit の内容、3 mode の announce、graceful stop が
  打ち消されないこと、quote / specifier escape、非 UTF-8 の拒否、config dir の注入、install の reload→enable 順序、
  write / systemctl / mkdir 失敗の伝播、uninstall の absent / present と削除失敗）
- `cargo test -p usagi --bin usagi runtime::launchd`: 7 件（refactor 後も pass）
- `ruby scripts/coverage-off-lint.rb`: ok
