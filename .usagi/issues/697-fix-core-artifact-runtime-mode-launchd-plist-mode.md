---
number: 697
title: fix(core): artifact に既定 runtime mode を焼き込み、launchd plist へ mode を伝える
status: todo
priority: high
labels: [core, build]
dependson: []
related: [693, 694, 542]
created_at: 2026-08-17T23:32:41.138204+00:00
updated_at: 2026-08-17T23:32:41.138204+00:00
---

設計は [document/proposals/17-v2-trial-channel.md](../../document/proposals/17-v2-trial-channel.md) の
「release build の既定 mode」が正本。

## 背景

`runtime_mode()` は `USAGI_RUNTIME_MODE` が無ければ **debug / release build とも `local`** を選ぶ。
production は明示指定を要求する。この非対称は意図的な fail-safe で、v2 は v1 が本番で使われている
同じマシン・同じ repository の中で開発されるため、`cargo run` や test が実 state を掴んではならない。

**この既定は試用にとっては正しいが、正式版にとっては正しくない。** `install.sh` も release artifact も
`USAGI_RUNTIME_MODE` を設定しないため、今のまま v2 を出荷すると**利用者のデータが `~/.usagi/local/` に入る**。
`local` は開発時の概念であって、利用者に見せる置き場所ではない。

### env では解決しない

[#542](542-fix-daemon-fence-workspace-mode-home.md) が記録しているとおり、利用者自身の shell を統一する
強制力は無い。env に依存した既定は「plain shell で起動したら別の世界に入る」を残す。

### launchd に mode が伝わらない

`daemon install-service` が書く plist は**環境変数を持たない**設計である（`src/runtime/launchd.rs` の
`render` と、その不在を固定する unit test）。したがって supervise される `daemon serve` は env 不在から
`local` を選ぶ。一方 plist の stderr log path は install した process の **mode-selected** data directory
から作る。この 2 つが食い違うため、production mode から `install-service` すると
**log は production 配下・daemon は local** という組み合わせになる。

## やること

### 1. artifact に既定 mode を焼き込む

| artifact | 既定 mode | 根拠 |
|---|---|---|
| source build（`cargo run` / `cargo test`） | `local` | 開発中に実 state を触らせない |
| beta channel artifact | `local` | 試用が可逆であること |
| stable artifact（`v3.0.0`） | `production` | 利用者のデータを `~/.usagi` 直下に置く |

- `RuntimeMode::from_env_value` の fallback が、この焼き込んだ既定を参照するようにする
  （現在は無条件に `Local`）。
- 焼き込みは cargo feature か `build.rs` が stamp する定数のどちらかにする。**既定は `local` 側**にして、
  production を選ぶほうを明示的な build 時の行為にする（fail-safe の向きを保つ）。
- `USAGI_RUNTIME_MODE` は引き続きすべてを上書きできる（開発・調査の経路を塞がない）。
- `paths.rs` の既存 test は env 由来の 3 mode を固定しているので、**焼き込んだ既定 × env 有無**の
  組み合わせを追加する。

### 2. launchd plist へ mode を伝える

plist が env を持たない現在の設計に対して、次のどちらかを選ぶ。**選択理由を同じ変更に記録する**。

- (a) mode だけを例外として plist に `EnvironmentVariables` で書く。
- (b) `daemon serve` に mode を渡す引数を足し、plist の `ProgramArguments` に載せる。

いずれでも、install した process の mode と supervise される daemon の mode、および stderr log path の
mode が**一致する**ことを固定する。現在の「plist に環境変数を書かない」を明示している
[5. daemon#launchd supervision](../../document/05-daemon.md#launchd-supervision) は、選んだ方に合わせて更新する。

### 3. docs

- [5. daemon#daemon data directory](../../document/05-daemon.md#daemon-data-directory) の
  「環境変数を未指定または不正な値にした場合は、debug / release build とも local を既定にする」を、
  artifact 既定を含む記述へ更新する。
- `document/09-env.md` の予約変数の扱いに影響が無いことを確認する。

## テスト

- `cargo test -p usagi-core`: 焼き込んだ既定 × env 有無 × 不正値の全組み合わせ。
- `cargo test -p usagi --bin usagi`: plist の render に mode が載ること、install した mode と
  log path の mode が一致すること。
- 既定を production に焼いた build で、`USAGI_RUNTIME_MODE` 未設定のまま `<base>` 直下を使うことを固定する。
