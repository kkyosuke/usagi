---
number: 651
title: docs: restart の拒否のみを説明する README/overview が実装の seamless rollover と食い違う
status: done
priority: medium
labels: [review, v2, docs, daemon]
dependson: []
related: []
created_at: 2026-08-05T01:02:16.964086+00:00
updated_at: 2026-08-05T09:05:24.313582+00:00
---

## 出典

先行する "uiux" review session（origin/main 3e21b392 時点、コード変更なしのレビュー）の finding 6。本 issue はその finding を再検証し起票したもの。

## Finding

`README.md`（218行目付近）と `document/01-overview.md`（55行目付近）は「`stop`/`restart` は live Agent や terminal があると拒否する。巻き添えにしてよい場合だけ `--force` を付ける」とだけ説明しており、拒否以外の経路を一切書いていない。

しかし実装（`crates/daemon/src/usecase/replacement.rs` の `enum ReplacementPlan { ColdTransition, SeamlessRollover, Refused { .. } }` と `plan_replacement`）は、live runtime があっても `SeamlessRefusal` に該当する具体的な前置条件（registry 未読、有効な active 未登録、generation 上限、draining collection 未処理など）が無い限り `SeamlessRollover` を選び、`execute_gated_rollover`（`crates/daemon/src/usecase/authority/rollover.rs`）で PTY を維持したまま handoff する。つまり**多くの場合 `restart` は拒否されず、PTY を保持したまま安全に切り替わる**。

`document/05-daemon.md` 自体もこの点で内部矛盾がある。142行目付近のコマンド一覧表と383〜439行目の「planned replacement」節は seamless rollover を正しく説明しているが、106行目・123〜124行目（ASCII図に「manual restart / replace, daemon owns live runtime → refused, effect 0」と記載）と518行目付近は rollover に触れず拒否のみを述べている。

README/overview の記述に従って `--force` を安易に付けると、実際には rollover で維持できたはずの PTY を不必要に破棄してしまうリスクがある。

## 影響

- ドキュメントの誤導によりユーザーが不必要に live session を失う可能性がある。
- コード変更は不要で、ドキュメント修正のみで解消できる。

## 修正方針

- `README.md` と `document/01-overview.md` の該当箇所を、`document/05-daemon.md` の実際の3分岐（cold transition / seamless rollover / refused）を要約する形に更新し、詳細は `05-daemon.md` にリンクする（SSoT 規約に従う）。
- `document/05-daemon.md` 自身の106行目・123〜124行目・518行目付近の記述も、同ファイル内の詳細な表（383〜439行目）と整合するよう修正する。

## 受け入れ条件

- README/overview/05-daemon.md のいずれを読んでも、restart が「拒否のみ」ではなく3分岐であることが一貫して分かる。
- Markdown link check（lychee）が通る。
