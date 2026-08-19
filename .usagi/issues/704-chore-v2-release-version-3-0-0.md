---
number: 704
title: chore: v2 の最初の公開 release として version を 3.0.0 へ上げる
status: done
priority: high
labels: [build]
dependson: []
related: [697, 699, 700]
created_at: 2026-08-19T22:20:53.747090+00:00
updated_at: 2026-08-19T22:20:53.747090+00:00
---

## 概要

v2 を公開 release として出すための version bump。ルート `Cargo.toml` の `version` 変更が
`auto-release.yml` の起点なので、**この変更が `main` に入った時点でリリースが公開される**。

## なぜ 3.0.0 か

| 理由 | 内容 |
|---|---|
| v1 を追い越す必要がある | 既存の公開 release は `v2.9.1`。これ以下の version でタグを切ると `/releases/latest` が v1 のままになり、`install.sh` の既定経路（`/releases/latest` ＋ 厳格な `^v?\d+\.\d+\.\d+$` filter）が新しい v2 を選ばない |
| `2.x` の延長では届かない | ルートは退避時点の v1 から引き継いだ `2.6.0` で、`2.7` / `2.8` / `2.9` 系では `2.9.1` を超えられない |
| major bump が実態に合う | v2 は別実装で、データ配置（`~/.usagi` 直下の構造）と daemon 中心の実行モデルが変わる |

## 前提（すべて `main` に入っている）

| 依存 | PR |
|---|---|
| リリース起点をルート `Cargo.toml` へ切り替え、3 platform を `--features production` で build | #1504 |
| artifact に既定 runtime mode を焼き込み、launchd へ data home を渡す | #1503 / [#697](697-fix-core-artifact-runtime-mode-launchd-plist-mode.md) |
| Linux の systemd user unit | #1507 / [#699](699-feat-daemon-linux-systemd-user-unit-service-macos.md) |
| supervised daemon の起動 directory を pin | #1519 / [#700](700-fix-daemon-supervised-daemon-directory-pin-unit-systemd.md) |

`--features production` が無い artifact は利用者のデータを `~/.usagi/local/` に入れてしまうため、
[#697](697-fix-core-artifact-runtime-mode-launchd-plist-mode.md) より前にリリースしてはいけなかった。

## マージ後に自動で起きること

1. `auto-release.yml` がルート `Cargo.toml` の version 変更を検知し、`v3.0.0` タグを対象に `release.yml` を呼ぶ
2. `release.yml` が Linux amd64 / macOS amd64 / macOS arm64 を `--features production` で release build する
3. `v3.0.0` タグと GitHub Release を作成し、各 archive に `.sha256` と `.version` を添付する
4. 以降 `curl -fsSL .../scripts/install.sh | bash` と `usagi update` が v2 を導入する

## やること

- ルート `Cargo.toml` の `version` を `2.6.0` → `3.0.0`
- `Cargo.lock` の `usagi` エントリを追随（`cargo check --workspace`）

manifest の diff はこの 2 行だけに保つ（PR にはこの issue ファイルも載る）。リリースの引き金となる
PR は、何が引き金かがひと目で分かる形にする。

## リリース後の確認

```bash
curl -fsSL https://raw.githubusercontent.com/KKyosuke/usagi/main/scripts/install.sh | bash
usagi --version     # usagi 3.0.0
```

## 既知の残件（リリースを止めない）

`document/01-overview.md#現在の実装状態` が「v2 は workspace の骨組みと、それを検証する最小の実行面を持つ」と
書いたままで、issue 621 件消化後の実態とかけ離れている。利用者が最初に読む箇所なので別 PR で更新する。
