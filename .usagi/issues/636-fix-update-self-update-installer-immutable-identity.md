---
number: 636
title: fix(update): self-update installer を immutable identity へ束縛する
status: done
priority: high
labels: [review, security, cli, update, release, supply-chain]
dependson: []
related: []
created_at: 2026-08-02T23:14:00.167300+00:00
updated_at: 2026-08-03T01:26:58.709111+00:00
---

## Finding（high / update trust boundary）

### 脅威モデルと対象

release archive/checksum/version artifactと、既存binaryを置換するinstaller codeは別のtrust objectである。releaseを発行せずmutable branchへwriteできる主体、branch compromise、配信途中のbranch更新をself-updateの実行権限として暗黙昇格させない。

`crates/cli/src/cli/commands/update.rs::install_command` は毎回 `https://raw.githubusercontent.com/<repo>/main/scripts/install.sh` を取得し、内容・commit identity・signatureを検証せず `bash` へpipeする。`src/runtime/cli.rs` はそのshellを実行する。

### 発生条件・影響・根拠

`main/scripts/install.sh` が既存releaseと無関係に変更・侵害された状態で利用者が `usagi update` を実行すると、archive checksum検証へ到達する前にmutable scriptが利用者権限で任意codeを実行する。現installer内のprivate staging、archive shape、SHA-256、candidate version、atomic renameは、installer自身のauthenticityを検証しない。

正本 `document/02-architecture.md` はplatform archiveとverification artifactsに基づくverified stagingをself-update契約としているが、bootstrap codeはrelease identityへ束縛されていない。

### effect-zero 条件

installerのimmutable identity/authenticityを検証できない場合、shell/process execution、downloaded candidate execution、target replacementを0件にして旧binary bytes/modeを保持する。version選択時も選んだreleaseとinstaller/checksum/versionのidentityを一組にする。

## 修正方針

- mutable `main` のremote scriptを直接実行しない。
- installerを現在binaryへ内蔵するか、選択releaseのimmutable tag/commit/artifactとして取得し、埋込/署名済みdigestで検証してから実行する。
- bootstrap installerとarchive/checksum/versionのrelease identityを結び、途中でbranch lookupしない。
- verification failureは非0でsafe messageだけを返す。

## 必要な回帰テスト

mutable main scriptがrelease後に変わってもself-updateが実行しない/参照しないこと、installer digest mismatch、tag/asset mismatch、response swap、truncated scriptでprocess effect 0かつ旧binary不変をhermetic fixtureで固定する。選択versionとlatestの両経路を含める。

## 既存 issue との差分
