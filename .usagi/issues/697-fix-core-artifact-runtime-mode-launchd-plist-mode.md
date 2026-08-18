---
number: 697
title: "fix(core): artifact に既定 runtime mode を焼き込み、launchd plist へ data home を渡す"
status: done
priority: high
labels: [core, daemon]
dependson: []
related: [542]
created_at: 2026-08-17T23:32:41.138204+00:00
updated_at: 2026-08-18T00:10:00.000000+00:00
---

## 概要

`USAGI_RUNTIME_MODE` 未指定時の既定 mode が env だけで決まっていたため、**配布 artifact が
利用者のデータを `~/.usagi/local/` へ入れる**状態だった。あわせて launchd plist が data home を
運ばないため、supervise される daemon が install 元と別の directory を掴む欠陥があった。

## 欠陥 1: release build も `local` を既定にする

`RuntimeMode::from_env_value` は未指定・不正値を無条件に `Local` へ落とし、`scripts/install.sh` も
release artifact も `USAGI_RUNTIME_MODE` を設定しない。したがって v2 をそのまま出荷すると、
利用者のデータが `~/.usagi/local/` に入る。`local` は開発時の隔離のための概念であって、
利用者に見せる置き場所ではない。

env では解決できない。[#542](542-fix-daemon-fence-workspace-mode-home.md) が記録しているとおり、
利用者自身の shell が常にこの変数を持つことは強制できず、env だけを既定の決め手にすると
「installer や service が意図した data home」と「plain shell で起動した `usagi` が解決する data home」が
食い違う余地が残る。

## 欠陥 2: launchd plist に data home が乗らない

`daemon install-service` が書く plist は環境変数を一切持たない設計だった。launchd は install した
shell の環境ではなく launchd 自身の環境で agent を起動するため、supervise される `daemon serve` は
data home を**空の環境から再解決**する。一方 plist の stderr log path は install した process の
**mode-selected** data directory から作る。この 2 つが食い違うため、production mode から
`install-service` すると **log は production 配下・daemon は local** という組み合わせになる。

## やったこと

### artifact に既定 mode を焼き込む

- `RuntimeMode::from_env_value(value, default)` に fallback を引数で渡す形へ変更した。既定を
  literal で埋めないことで、**(env 値 × 既定) の全組み合わせを 1 つの build でテストできる**。
- `paths::DEFAULT_RUNTIME_MODE` を artifact 定数として追加した。ルートパッケージの `production`
  feature（`usagi-core` の `production-runtime-mode` を有効にする）で `Production` を、feature 無しの
  既定では `Local` を選ぶ。**危険な向き（本番 state を書く）に明示的な build 時の指定を要求する**
  非対称を保つため、既定は `local` 側に置いた。
- `from_env_value` に `Some("local")` の明示 arm を追加した。これが無いと、production 既定の
  artifact が利用者の明示的な `USAGI_RUNTIME_MODE=local` を production へ格上げしてしまう。
- `USAGI_RUNTIME_MODE` は artifact 既定を両方向へ上書きできるままにした（開発・調査の経路を塞がない）。

### launchd plist へ data home の組を渡す

- `launchd::install` / `install_with` / `render` が `&Path`（selected dir）ではなく
  `&DataHome`（base + mode の組）を受け取る形へ変更した。log path は `selected()`、plist の
  `EnvironmentVariables` は base と mode の綴り、という 1 つの `DataHome` から両方を導く。
- 運ぶのは **`USAGI_HOME` と `USAGI_RUNTIME_MODE` の 2 つだけ**で、token・credential・session state は
  書かない。この組を渡す契約は、daemon が Agent MCP child に渡すものと同一である。
- base が UTF-8 で綴れない場合は lossy 変換せず `InvalidInput` で install を拒否する
  （壊れた base を書いた plist は、誰も選んでいない directory へ daemon を送る）。
- 呼び出し側は `DataHome::from_selected(&data_dir, runtime_mode())` で組を復元する。production は
  selected が base 自体なので、`parent()` による推測ではなく組で扱う必要がある。

### docs

- [5. daemon#daemon data directory](../../document/05-daemon.md#daemon-data-directory) の既定 mode の記述を
  artifact 既定へ更新し、`### artifact の既定 mode` を追加した。3 つの綴りが明示指定として尊重されることも明記した。
- [5. daemon#launchd supervision](../../document/05-daemon.md#launchd-supervision) の「plist に環境変数を
  保存しない」を、data home の組だけを運ぶ契約とその理由へ更新した。

## テスト

- `cargo test -p usagi-core`: 明示 3 綴り × 既定 3 mode の round trip、未指定・不正値・空文字の fallback、
  artifact 既定定数、`runtime_mode()` の env 分岐。
- `cargo test -p usagi --bin usagi`: plist が data home の組を announce すること、production で base が
  base 自体であること、base の XML escape、非 UTF-8 base の拒否、install plan の log path が
  selected directory であること。
- `cargo test -p usagi --test mcp_e2e`: Agent child の mode 再適用が artifact 既定に依存しないこと。
