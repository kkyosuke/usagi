---
number: 706
title: fix(sandbox): Linux で agent global config の prefix 相当を writable にする
status: done
priority: medium
labels: []
dependson: []
related: [705]
created_at: 2026-08-19T22:52:52.155223+00:00
updated_at: 2026-08-26T00:00:00+00:00
---

## 背景

#705 で、Claude の global config（`~/.claude.json`）を writable にする grant を launcher に足した。macOS の
`sandbox-exec` は path policy なので `(allow file-write* (regex #"^<prefix>"))` で本体・`.lock`・
`.tmp.<pid>.<random>`・`.backup.<ms>` をまとめて許可できる。

Linux の `bwrap` は **mount 単位**でしか許可を表現できず、prefix を表現できない。現状は
`--bind-try ~/.claude.json` で config 本体だけを read-write に再 bind しているが、`$HOME` directory 自体は
read-only のままなので、**lock / temp を `$HOME` 直下に作って rename で被せる保存経路は Linux では通らない**。
その結果、Linux では #705 の症状（folder trust と MCP 承認が毎起動やり直しになる）が残る。

## 検討する案

| 案 | 内容 | 懸念 |
|---|---|---|
| `$HOME` を writable にし、既存 entry を ro-bind し直す | `--bind-try $HOME $HOME` のあとに `$HOME` の直下 entry を `--ro-bind-try` で戻し、config prefix に該当する entry だけ writable のままにする | 起動時に `$HOME` の entry を列挙する IO が要る（純粋な計画部へ列挙結果を渡す形にする）。新規 dotfile の作成は許してしまう |
| config だけを別 directory へ寄せる | `CLAUDE_CONFIG_DIR` を `~/.claude` に向ける | 利用者本人の設定と分岐する（#702 で外した方針に戻る） |

## 完了条件

- Linux で sandboxed の Claude が folder trust を受理したあと、`~/.claude.json` の `projects[<cwd>]` が保存される
  ことを実機または CI で確認する。
- macOS 側の grant（regex prefix）と挙動が揃い、`document/02-architecture.md` の
  「agent global config の writable prefix」節から Linux の制限記述を落とせる。

## 2026-08-26 対応

- [x] Linux launcher が起動直前の `$HOME` 直下 entry を列挙し、純粋な sandbox plan へ渡す。
- [x] `bwrap` は `$HOME` を read-write bind した後、config prefix family 以外の既存 entry を
      read-only に戻し、agent state / launch root だけを最後に writable で重ねる。
- [x] entry inventory の欠落・列挙失敗・直下でない path は fail closed にする。
- [x] `.claude.json` の lock / temp / backup / rename 保存と、他の既存 HOME entry の拒否を
      shipping launcher E2E と Linux argv unit test で固定した。
- [x] macOS の regex grant は維持し、architecture の Linux 制限記述を実装済み契約へ更新した。
