---
number: 578
title: feat(tui): Workspace Agent の New CLI picker と root Agent launch を追加する
status: todo
priority: high
labels: [v2, tui, agent, ux]
dependson: [577]
related: [388, 506, 510, 545]
parent: 571
created_at: 2026-07-27T23:04:31.429786+00:00
updated_at: 2026-07-27T23:04:31.429786+00:00
---

## 背景

#576 が Workspace Agent drawer shell、#577 がroot Agent conversationの復元・runtime接続を提供する。本 issue はdrawerの `New` affordanceを、install済みAgent CLIの明示pickerとdaemon-authoritativeなroot Agent launchへ接続する。

新規作成だけを独立させ、既存conversationの復元・resumeと、新しいprovider conversationのlaunchを同じ変更で混ぜない。

## 対象責務

- drawerの`New`からAgent CLI pickerを開く。候補は合成ルートが注入する`AvailableModels`に含まれるinstall済みCLIだけ（現 vocabulary: `claude` / `codex` / `sakana.ai`）。
- configの`default_model`がinstall済みなら初期highlightにする。未installなら最初のinstall済み候補をhighlightするだけで、自動確定しない。
- ↑↓または同等の選択入力とEnterでCLIを確定し、Esc/Cancelでconversation/order/selectionを変えずdrawerへ戻る。
- install済みCLIが0件ならpickerを空で開かず、installation/configを促すsafe empty stateを表示する。daemon requestは送らない。
- 確定時はworkspace root scope（`session_id: None`）、新しい`OperationId`、選択したexplicit profileで既存daemon Agent launch pathを1回だけ呼ぶ。TUIはargv/model path/secret/cwdを組み立てない。
- request発行前にroot Agent pending slotを1枚だけ作り、matching operation/root scope/semantic intentを持つsuccessful finalのexact `TerminalRef`だけを同slotのlive Agentへ昇格する。
- double Enter、duplicate accepted/final、reconnect replayはoperation fenceで1 request / 1 spawn / 1 tabへ収束させる。
- launch成功後は新しいconversationをselectedにしてroot `AgentTabIntent`へorder/selectionをatomic commitする。persist失敗時の可視state/daemon runtimeの扱いを既存intent契約と整合させ、successを捏造しない。
- daemon不通、profile rejection、configured default不在、stale/wrong-workspace/wrong-session final、operation mismatch、future intent schemaをfail closedにし、既存conversation/selection/bytesを壊さない。
- drawer以外のmanaged-session `agent -m` とConfigのCLI vocabulary/default挙動を維持する。

## 非対象

- 既存root Agentのrestore/attach/resume/order/dismissal（#577）。
- CLI installation自体、認証設定、provider model一覧。
- provider transcriptの独自chat renderer。
- shipping process E2Eと全体docs確定（#579）。

## 受入条件

- [ ] `New`は必ずinstall済みCLI pickerを経由し、選択前にlaunchしない。
- [ ] default installed / default missing / 1候補 / 複数候補 / 0候補で初期selectionとempty stateが決定的である。
- [ ] Cancel/Escは既存conversation、order、selection、drawer open状態を変更しない。
- [ ] 選んだexplicit profileとroot scopeでdaemon requestを1回だけ送り、TUIはargv/cwd/provider ID/secretを扱わない。
- [ ] pending→liveはmatching operationとroot exact `TerminalRef`のfinalだけを受け、duplicate/double submit/replayが1 spawn/1tabへ収束する。
- [ ] failure/wrong-scope/stale final/persist failure/future schemaでlocal spawn・空conversation・既存selection破壊を起こさない。
- [ ] managed-session Closeup `agent -m` とConfigのinstall/default vocabularyに回帰がない。

## 必須テスト

- picker reducer/presentation: candidate order、default highlight、0/1/N、keyboard、cancel、狭幅/CJK。
- launch adapter/runtime: explicit profile、root scope、pending、accepted/final、double submit、duplicate/stale/wrong-scope final、transport failure。
- intent persistence: successful selection commit、write/CAS/future-schema failure rollback。
- integration: fixture CLIごとのroot launch、spawn count 1、drawer close/reopenでsame exact tab。shipping PTYの最終受入は#579。
