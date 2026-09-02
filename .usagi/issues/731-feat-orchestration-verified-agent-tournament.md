---
number: 731
title: feat(orchestration): Verified Agent Tournament で複数候補を証拠ベース比較する
status: todo
priority: medium
labels: [product, orchestration, supervisor, verification, evaluation, differentiation]
dependson: [729]
related: [324, 327, 730]
created_at: 2026-09-03T08:34:08+09:00
updated_at: 2026-09-03T08:34:08+09:00
---

## 目的

同じ task を複数の Agent / runtime / model に隔離環境で実行させ、自己申告や生成速度だけでなく、
共通の artifact contract と #729 の verification evidence に基づいて候補を比較・選択できるようにする。

## 背景

usagi は複数 session / Agent、managed worktree、durable supervisor、bounded execution policy を持つが、
同一の入力条件から複数候補を生成し、検証結果・差分・費用を同じ尺度で比較する workflow はない。

単純な「一番早く完了を申告した Agent」を勝者にすると品質と再現性を損なう。競争実行そのものではなく、
同じ base revision・task contract・verification policy に束縛された verified candidate の比較を製品契約にする。

## スコープ

### Tournament specification

- root task、required artifact contract、base commit、verification policy revision、候補 profile、最大候補数、
  wall-clock / concurrency / retry / cost budget を持つ versioned tournament specification を定義する。
- 各候補を別 session / managed worktree / supervisor task に割り当て、repository mutation と runtime identity を隔離する。
- 候補追加、retry、resume は同じ tournament generation と exact base identity に fence する。
- malformed specification、利用不能 profile、候補数・budget 超過は effect 実行前に安全に拒否する。

### Verification と比較

- すべての候補へ同じ required gate を適用し、#729 で Verified Done になった候補だけを選択対象にする。
- hard gate の成否を最優先し、その後に policy が宣言した deterministic metric
  （例: optional gate、変更量、所要時間、消費 budget）を適用する。
- score、tie-break、失格理由を versioned rubric として保存し、後から同じ evidence で再計算できるようにする。
- LLM judge や Agent の自己評価だけを合否・勝者の唯一の根拠にしない。主観評価を使う場合は参考値として
  provenance と不確実性を明示する。
- 同点、比較不能、verified candidate 不在は human escalation にし、暗黙の勝者を作らない。

### Lifecycle と利用者 surface

- tournament、candidate、verification、selection の状態を durable にし、daemon restart、重複 completion、
  late result、candidate cancel に対して冪等に収束させる。
- budget 上限または十分な verified candidate を得た場合の bounded early-stop policy を定義する。
- TUI で候補ごとの status、diff summary、gate outcome、score、費用・時間、失格理由を比較できる。
- MCP から tournament の start / get / list / cancel、candidate evidence、selection request を操作・取得できる。
- 選択後も全 candidate の provenance と Evidence Bundle を保持し、採用候補を明示的な後続 action へ渡す。

## 受け入れ条件

- 全候補が同じ base commit、artifact contract、verification policy revision から開始する。
- 候補の filesystem / process / session state が隔離され、相互の変更を読み書きしない。
- verification 未完了・failed・stale evidence の候補は score にかかわらず選択されない。
- 同じ evidence と rubric から同じ順位・tie・失格理由が得られる。
- retry、restart、late / duplicate result、cancel で候補や budget が二重計上されない。
- verified candidate 不在、同点、rubric failure は bounded に escalation へ収束する。
- TUI と MCP が同じ tournament revision と comparison result を表示する。
- secret、prompt 本文、raw output、provider-native ID を比較画面・event・bundle に露出しない。
- 既存の単一 Agent / supervisor workflow を破壊しない。
- 仕様ドキュメントと README の利用例を更新し、unit / integration / restart-replay test と coverage 100% を維持する。

## 非目標

- 公開 benchmark や runtime / model の恒久的ランキングサービス。
- LLM judge 単独による自動採用。
- 複数候補の patch を自動合成すること。
- 選択候補の PR 自動 merge。
- 異なる base revision や異なる required gate の候補を同列に比較すること。
