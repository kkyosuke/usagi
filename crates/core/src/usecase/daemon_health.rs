//! 診断専用の daemon health projection。
//!
//! [`DaemonMetrics`] は表示専用の process-local counter であり、操作の可否や
//! resource ownership の権威ではない。この module が作る [`DaemonHealth`] も同じ
//! 位置づけで、**診断（利用者へ「daemon 側が劣化している」と伝えること）にしか
//! 使わない**。reducer state に載せず、Effect を生まず、command・fence・ownership の
//! 判定にも参加しない。したがって health が誤って警告しても、失われるのは表示の
//! 静けさだけである。
//!
//! # cumulative counter を actionable に読む
//!
//! `DaemonMetrics` の counter は daemon process の生存期間で単調増加する。値そのものや
//! 「一度でも増えたか」で判定すると、**一度警告したら二度と消えない indicator** に
//! なる。さらに `terminal_dropped_bytes` は端末 1 本の retention window（64KiB）を
//! 超えた分を捨てた量で、忙しい agent では常時増える通常動作である。
//!
//! そのため [`DaemonHealthTracker`] は次の 4 つを守る。
//!
//! | 規則 | 実装 |
//! |---|---|
//! | rate で見る | sample 間の差分を経過時間で割った毎秒レートを閾値と比べる |
//! | 連続で見る | 閾値超えが規定 sample 数続いて初めて点灯する |
//! | 減衰する | 点灯は最後の該当 sample から [`HOLD_MS`] で自然に消える |
//! | 再 baseline する | 再起動・時刻後退・schema 変化・観測の空白では差分を取らない |
//!
//! 時計は sample 自身の `sampled_at_ms` だけを使う。実時計を読むのは
//! [`DaemonHealthTracker::evaluate`] の引数だけで、この module は IO を持たない。

use crate::usecase::client::DaemonMetrics;

/// 最新 sample がこれ以上古ければ「観測が停滞している」と扱う。
///
/// metrics lane の cadence は 1s で、失敗中は 8s 上限の指数 backoff に入る
/// （`document/03-tui.md`）。単発の失敗は 2s 程度の遅れにしかならないため、
/// 6s は「連続して失敗している」ことを意味する。
pub const STALLED_MS: u64 = 6_000;

/// 最新 sample がこれ以上古ければ「daemon が応答していない」と扱う。
///
/// backoff の上限（8s）を大きく超えるため、lane の一時的な失敗では到達しない。
pub const UNRESPONSIVE_MS: u64 = 30_000;

/// 差分を判定に使える sample 間隔の上限。
///
/// これを超える空白は再接続・停滞明けであり、そこを跨いだ差分は「いつの事象か」を
/// 表さない。空白のあとの 1 発目は baseline を引き直すだけで、警告にはならない。
const BASELINE_GAP_MS: u64 = 5_000;

/// 点灯を保持する時間。事象が止まれば indicator はこの時間で消える。
pub const HOLD_MS: u64 = 10_000;

/// 点灯に必要な連続超過 sample 数（バーストを弾く）。
const SUSTAINED_SAMPLES: u8 = 3;

/// 1 回でも起きたら報告する事象に使う連続数。
const IMMEDIATE_SAMPLES: u8 = 1;

/// 端末出力の欠落が「retention window の通常動作」を超えたと見なすレート。
const TERMINAL_DROPPED_BYTES_PER_SEC: u64 = 1024 * 1024;

/// PTY reader が queue の空きを待った量が「バースト」を超えたと見なすレート。
const TERMINAL_BACKPRESSURED_BYTES_PER_SEC: u64 = 256 * 1024;

/// metrics tick の取りこぼしが継続的だと見なすレート（毎秒 1 件）。
const METRICS_UPDATES_PER_SEC: u64 = 1;

/// 差分がありさえすれば報告する閾値（PR scan の取りこぼし）。
const ANY_INCREASE: u64 = 0;

/// indicator の強さ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HealthLevel {
    /// 静か。正常時は画面に何も足さない。
    Ok,
    /// 要注意。劣化しているが利用は続けられる。
    Warning,
    /// 異常。daemon が観測できていない。
    Danger,
}

/// indicator に出す理由。
///
/// **閉じた enum であり free text を持たない**。したがって secret・raw な PTY 出力・
/// path が indicator に載ることが構造的に起こり得ない。表示文言は presentation 層が
/// この値から決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthReason {
    /// 観測済みの daemon から [`UNRESPONSIVE_MS`] 以上新しい sample が来ていない。
    DaemonUnresponsive,
    /// 最新 sample が [`STALLED_MS`] 以上古い（metrics lane が失敗し続けている）。
    MetricsStalled,
    /// 端末出力が retention window から通常動作を超える速さで捨てられている。
    TerminalOutputDropped,
    /// PTY reader が bounded queue の空きを待ち続けている。
    TerminalBackpressure,
    /// 確定済み出力が PR scan されずに捨てられた（PR 検出の取りこぼし）。
    PrScanIncomplete,
    /// metrics tick が購読側に届かず捨てられ続けている。
    MetricsUpdatesDropped,
}

/// health の判定結果。`Ok` は理由を持たないため、無効な組み合わせを表現できない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonHealth {
    Ok,
    Warning(HealthReason),
    Danger(HealthReason),
}

impl DaemonHealth {
    /// indicator の強さ。
    #[must_use]
    pub const fn level(&self) -> HealthLevel {
        match self {
            Self::Ok => HealthLevel::Ok,
            Self::Warning(_) => HealthLevel::Warning,
            Self::Danger(_) => HealthLevel::Danger,
        }
    }

    /// 表示する理由。`Ok` では `None`。
    #[must_use]
    pub const fn reason(&self) -> Option<HealthReason> {
        match self {
            Self::Ok => None,
            Self::Warning(reason) | Self::Danger(reason) => Some(*reason),
        }
    }

    /// 静かにしておくべきか。
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }
}

/// 直前に観測した sample の identity。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Observed {
    sample_at_ms: u64,
    schema_version: u16,
}

/// 1 つの cumulative counter に対する rate 判定と減衰。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct RateGate {
    baseline: Option<u64>,
    streak: u8,
    hot_until_ms: Option<u64>,
}

impl RateGate {
    /// 1 sample を畳む。`window_ms` が `None` のときは baseline だけ引き直す。
    fn observe(
        &mut self,
        value: u64,
        sample_at_ms: u64,
        window_ms: Option<u64>,
        limit_per_sec: u64,
        sustained: u8,
    ) {
        let previous = self.baseline.replace(value);
        // 比較できる窓が無い（初回・再接続の空白・時刻後退）ときは差分を取らない。
        let (Some(window_ms), Some(previous)) = (window_ms, previous) else {
            self.streak = 0;
            return;
        };
        // counter が後退した = 別 process の daemon が publish している。
        let Some(delta) = value.checked_sub(previous) else {
            self.streak = 0;
            return;
        };
        // `window_ms` は呼び出し側が `sample_at_ms` の前進を確認済みなので 0 ではない。
        let rate_per_sec = u128::from(delta) * 1_000 / u128::from(window_ms);
        if delta == 0 || rate_per_sec < u128::from(limit_per_sec) {
            self.streak = 0;
            return;
        }
        self.streak = self.streak.saturating_add(1);
        if self.streak >= sustained {
            self.hot_until_ms = Some(sample_at_ms.saturating_add(HOLD_MS));
        }
    }

    /// `now_ms` 時点で点灯しているか。
    fn is_hot(self, now_ms: u64) -> bool {
        self.hot_until_ms.is_some_and(|until_ms| now_ms < until_ms)
    }
}

/// sample 列を畳んで health を作る、純粋な観測器。
///
/// `Clone + PartialEq + Eq` なので TUI の frame material に載せられる（同じ material なら
/// 同じ frame という等式を壊さない）。observe は sample の時計しか読まず、実時計は
/// [`Self::evaluate`] の引数として外から渡す。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DaemonHealthTracker {
    observed: Option<Observed>,
    terminal_dropped: RateGate,
    terminal_backpressured: RateGate,
    pr_dropped: RateGate,
    pr_gaps: RateGate,
    metrics_updates: RateGate,
}

impl DaemonHealthTracker {
    /// 1 つの snapshot を畳む。
    ///
    /// 同じ sample を何度渡しても状態は変わらない（composition root の port は lane が
    /// 失敗している間、直前 sample を毎 frame 返し続ける）。新しい `sampled_at_ms` だけを
    /// 新しい観測として扱うため、frame rate は判定に影響しない。
    pub fn observe(&mut self, metrics: &DaemonMetrics) {
        let sample_at_ms = metrics.sampled_at_ms;
        let window_ms = match self.observed {
            // 再配達された同じ sample。streak も減衰も進めない。
            Some(observed)
                if observed.sample_at_ms == sample_at_ms
                    && observed.schema_version == metrics.schema_version =>
            {
                return;
            }
            Some(observed)
                if observed.schema_version == metrics.schema_version
                    && sample_at_ms > observed.sample_at_ms =>
            {
                let gap_ms = sample_at_ms - observed.sample_at_ms;
                (gap_ms <= BASELINE_GAP_MS).then_some(gap_ms)
            }
            // 初回、時刻後退（別 incarnation）、schema 変化（counter の意味が変わる）。
            _ => None,
        };
        self.observed = Some(Observed {
            sample_at_ms,
            schema_version: metrics.schema_version,
        });
        self.terminal_dropped.observe(
            metrics.terminal_dropped_bytes,
            sample_at_ms,
            window_ms,
            TERMINAL_DROPPED_BYTES_PER_SEC,
            SUSTAINED_SAMPLES,
        );
        self.terminal_backpressured.observe(
            metrics.terminal_backpressured_bytes,
            sample_at_ms,
            window_ms,
            TERMINAL_BACKPRESSURED_BYTES_PER_SEC,
            SUSTAINED_SAMPLES,
        );
        self.pr_dropped.observe(
            metrics.pr_projection_dropped_bytes,
            sample_at_ms,
            window_ms,
            ANY_INCREASE,
            IMMEDIATE_SAMPLES,
        );
        self.pr_gaps.observe(
            metrics.pr_projection_gaps,
            sample_at_ms,
            window_ms,
            ANY_INCREASE,
            IMMEDIATE_SAMPLES,
        );
        self.metrics_updates.observe(
            metrics.dropped_updates,
            sample_at_ms,
            window_ms,
            METRICS_UPDATES_PER_SEC,
            SUSTAINED_SAMPLES,
        );
    }

    /// `now_ms`（epoch ミリ秒）時点の health。
    ///
    /// **一度も観測していない状態は `Ok`** である。daemon が居なくても workspace は
    /// 動作するため（`document/03-tui.md`）、これは異常ではない。
    /// freshness の理由は counter 由来の理由より優先する。停滞した snapshot から
    /// 計算したレートを表示しないためである。
    #[must_use]
    pub fn evaluate(&self, now_ms: i64) -> DaemonHealth {
        let Some(observed) = self.observed else {
            return DaemonHealth::Ok;
        };
        // epoch 前の時計は「最も古い時刻」として扱う（age は 0 に飽和する）。
        let now_ms = u64::try_from(now_ms).unwrap_or(0);
        let age_ms = now_ms.saturating_sub(observed.sample_at_ms);
        if age_ms >= UNRESPONSIVE_MS {
            return DaemonHealth::Danger(HealthReason::DaemonUnresponsive);
        }
        if age_ms >= STALLED_MS {
            return DaemonHealth::Warning(HealthReason::MetricsStalled);
        }
        [
            (self.terminal_dropped, HealthReason::TerminalOutputDropped),
            (
                self.terminal_backpressured,
                HealthReason::TerminalBackpressure,
            ),
            (self.pr_dropped, HealthReason::PrScanIncomplete),
            (self.pr_gaps, HealthReason::PrScanIncomplete),
            (self.metrics_updates, HealthReason::MetricsUpdatesDropped),
        ]
        .into_iter()
        .find_map(|(gate, reason)| gate.is_hot(now_ms).then_some(DaemonHealth::Warning(reason)))
        .unwrap_or(DaemonHealth::Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DaemonHealth, DaemonHealthTracker, HOLD_MS, HealthLevel, HealthReason, STALLED_MS,
        UNRESPONSIVE_MS,
    };
    use crate::usecase::client::DaemonMetrics;

    const MIB: u64 = 1024 * 1024;

    fn sample(sampled_at_ms: u64) -> DaemonMetrics {
        DaemonMetrics {
            schema_version: 3,
            sampled_at_ms,
            cpu_percent_hundredths: 120,
            resident_memory_bytes: 32 * MIB,
            active_subscribers: 1,
            dropped_updates: 0,
            terminal_dropped_bytes: 0,
            terminal_coalesced_bytes: 0,
            terminal_backpressured_bytes: 0,
            pr_projection_dropped_bytes: 0,
            pr_projection_coalesced_bytes: 0,
            pr_projection_gaps: 0,
            // health は Agent concurrency を読まない（診断は counter と freshness だけ）。
            agent_concurrency: None,
        }
    }

    /// 1s 間隔で `count` 件の sample を畳む。各 sample は `field` を `step` ずつ増やす
    /// （`step` が 0 なら値を据え置いた「静かな」sample になる）。最後の sample 時刻と
    /// 累計値を返す。
    fn observe_series(
        tracker: &mut DaemonHealthTracker,
        start_ms: u64,
        count: u64,
        from: u64,
        step: u64,
        field: fn(&mut DaemonMetrics, u64),
    ) -> (u64, u64) {
        let mut total = from;
        for index in 1..=count {
            let mut metrics = sample(start_ms + index * 1_000);
            total += step;
            field(&mut metrics, total);
            tracker.observe(&metrics);
        }
        (start_ms + count * 1_000, total)
    }

    /// sample 時刻（`u64` ミリ秒）を実時計の引数型へ移す。
    fn at(ms: u64) -> i64 {
        i64::try_from(ms).expect("test clock fits in i64")
    }

    fn dropped_bytes(metrics: &mut DaemonMetrics, total: u64) {
        metrics.terminal_dropped_bytes = total;
    }

    #[test]
    fn an_unobserved_daemon_stays_quiet() {
        let tracker = DaemonHealthTracker::default();
        // daemon 不在の workspace は正常。indicator を出さない。
        assert_eq!(tracker.evaluate(1_000_000), DaemonHealth::Ok);
        assert!(tracker.evaluate(i64::MAX).is_ok());
    }

    #[test]
    fn a_healthy_sample_stays_quiet() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(10_000));
        assert_eq!(tracker.evaluate(10_500), DaemonHealth::Ok);
    }

    #[test]
    fn a_stalled_lane_warns_and_then_reports_an_unresponsive_daemon() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(100_000));

        assert_eq!(
            tracker.evaluate(at(100_000 + STALLED_MS - 1)),
            DaemonHealth::Ok
        );
        assert_eq!(
            tracker.evaluate(at(100_000 + STALLED_MS)),
            DaemonHealth::Warning(HealthReason::MetricsStalled)
        );
        assert_eq!(
            tracker.evaluate(at(100_000 + UNRESPONSIVE_MS)),
            DaemonHealth::Danger(HealthReason::DaemonUnresponsive)
        );
        // 新しい sample が来れば静けさに戻る。
        tracker.observe(&sample(100_000 + UNRESPONSIVE_MS));
        assert_eq!(
            tracker.evaluate(at(100_000 + UNRESPONSIVE_MS)),
            DaemonHealth::Ok
        );
    }

    #[test]
    fn a_clock_before_the_epoch_is_treated_as_the_oldest_time() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(100_000));
        // 負の now は 0 に丸め、age は飽和して 0 になる（未来の sample と同じ扱い）。
        assert_eq!(tracker.evaluate(-1), DaemonHealth::Ok);
    }

    #[test]
    fn a_sample_from_the_future_does_not_report_staleness() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(500_000));
        assert_eq!(tracker.evaluate(400_000), DaemonHealth::Ok);
    }

    #[test]
    fn routine_retention_trimming_never_warns() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(0));
        // 64KiB / 秒の trimming は retention window の通常動作。10 sample 続いても静か。
        let (now, _) = observe_series(&mut tracker, 0, 10, 0, 64 * 1024, dropped_bytes);
        assert_eq!(tracker.evaluate(at(now)), DaemonHealth::Ok);
    }

    #[test]
    fn a_single_output_burst_does_not_warn() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(0));
        // 閾値超えが 1 回だけでは点灯しない（連続 3 sample が必要）。
        let (now, _) = observe_series(&mut tracker, 0, 2, 0, 4 * MIB, dropped_bytes);
        assert_eq!(tracker.evaluate(at(now)), DaemonHealth::Ok);
    }

    #[test]
    fn a_sustained_output_flood_warns_and_then_decays() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(0));
        let (hot_at, total) = observe_series(&mut tracker, 0, 3, 0, 4 * MIB, dropped_bytes);
        assert_eq!(
            tracker.evaluate(at(hot_at)),
            DaemonHealth::Warning(HealthReason::TerminalOutputDropped)
        );
        // 事象が止まれば hold 窓の経過で消える。counter は増えたままで減らない。
        // 停滞と区別するため、静かな sample を届け続けながら hold 窓を越える。
        let quiet = HOLD_MS / 1_000 + 1;
        let (now, _) = observe_series(&mut tracker, hot_at, quiet, total, 0, dropped_bytes);
        assert!(now >= hot_at + HOLD_MS);
        assert_eq!(tracker.evaluate(at(now)), DaemonHealth::Ok);
    }

    #[test]
    fn sustained_backpressure_warns() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(0));
        let (now, _) = observe_series(&mut tracker, 0, 3, 0, 512 * 1024, |metrics, total| {
            metrics.terminal_backpressured_bytes = total;
        });
        assert_eq!(
            tracker.evaluate(at(now)),
            DaemonHealth::Warning(HealthReason::TerminalBackpressure)
        );
    }

    #[test]
    fn sustained_metrics_tick_loss_warns() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(0));
        let (now, _) = observe_series(&mut tracker, 0, 3, 0, 1, |metrics, total| {
            metrics.dropped_updates = total;
        });
        assert_eq!(
            tracker.evaluate(at(now)),
            DaemonHealth::Warning(HealthReason::MetricsUpdatesDropped)
        );
    }

    #[test]
    fn a_single_pr_scan_loss_warns_immediately() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(0));
        // PR projection queue の満杯は通常動作ではないため、1 回で報告する。
        let mut dropped = sample(1_000);
        dropped.pr_projection_dropped_bytes = 1;
        tracker.observe(&dropped);
        assert_eq!(
            tracker.evaluate(1_000),
            DaemonHealth::Warning(HealthReason::PrScanIncomplete)
        );

        // gap だけが増えた場合も同じ理由で報告する。
        let mut fresh = DaemonHealthTracker::default();
        fresh.observe(&sample(0));
        let mut gapped = sample(1_000);
        gapped.pr_projection_gaps = 2;
        fresh.observe(&gapped);
        assert_eq!(
            fresh.evaluate(1_000),
            DaemonHealth::Warning(HealthReason::PrScanIncomplete)
        );
    }

    #[test]
    fn freshness_outranks_a_counter_reason() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(0));
        let (hot_at, _) = observe_series(&mut tracker, 0, 3, 0, 4 * MIB, dropped_bytes);
        // 停滞中は counter 由来の理由を出さない（古い snapshot のレートだから）。
        assert_eq!(
            tracker.evaluate(at(hot_at + STALLED_MS)),
            DaemonHealth::Warning(HealthReason::MetricsStalled)
        );
    }

    #[test]
    fn a_redelivered_sample_does_not_advance_the_judgement() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(0));
        let mut flood = sample(1_000);
        flood.terminal_dropped_bytes = 4 * MIB;
        // 同じ sample を何度配達されても、超過は 1 回ぶんにしか数えない。
        for _ in 0..10 {
            tracker.observe(&flood);
        }
        assert_eq!(tracker.evaluate(1_000), DaemonHealth::Ok);
    }

    #[test]
    fn a_restarted_daemon_rebaselines_instead_of_warning() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(0));
        observe_series(&mut tracker, 0, 2, 0, 4 * MIB, dropped_bytes);
        // counter が 0 へ戻る = 別 process の broker。差分を取らず baseline を引き直す。
        let restarted = sample(3_000);
        tracker.observe(&restarted);
        let mut next = sample(4_000);
        next.terminal_dropped_bytes = 4 * MIB;
        tracker.observe(&next);
        assert_eq!(tracker.evaluate(4_000), DaemonHealth::Ok);
    }

    #[test]
    fn a_reconnect_gap_rebaselines_the_first_sample() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(0));
        // 30s の空白を跨いだ 1 発目は、大きな差分でも警告にしない。
        let mut resumed = sample(30_000);
        resumed.terminal_dropped_bytes = 512 * MIB;
        resumed.pr_projection_gaps = 9;
        tracker.observe(&resumed);
        assert_eq!(tracker.evaluate(30_000), DaemonHealth::Ok);
    }

    #[test]
    fn a_sample_clock_regression_rebaselines() {
        let mut tracker = DaemonHealthTracker::default();
        let mut first = sample(10_000);
        first.pr_projection_gaps = 1;
        tracker.observe(&first);
        let mut earlier = sample(2_000);
        earlier.pr_projection_gaps = 7;
        tracker.observe(&earlier);
        assert_eq!(tracker.evaluate(2_000), DaemonHealth::Ok);
    }

    #[test]
    fn a_schema_change_rebaselines() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(0));
        // 同じ時刻・別 schema は「別の counter 語彙」なので差分を取らない。
        let mut migrated = sample(0);
        migrated.schema_version = 3;
        migrated.pr_projection_dropped_bytes = 4;
        tracker.observe(&migrated);
        assert_eq!(tracker.evaluate(0), DaemonHealth::Ok);
    }

    #[test]
    fn health_exposes_its_level_and_reason() {
        assert_eq!(DaemonHealth::Ok.level(), HealthLevel::Ok);
        assert_eq!(DaemonHealth::Ok.reason(), None);
        assert!(DaemonHealth::Ok.is_ok());

        let warning = DaemonHealth::Warning(HealthReason::MetricsStalled);
        assert_eq!(warning.level(), HealthLevel::Warning);
        assert_eq!(warning.reason(), Some(HealthReason::MetricsStalled));
        assert!(!warning.is_ok());

        let danger = DaemonHealth::Danger(HealthReason::DaemonUnresponsive);
        assert_eq!(danger.level(), HealthLevel::Danger);
        assert_eq!(danger.reason(), Some(HealthReason::DaemonUnresponsive));
        assert!(HealthLevel::Danger > HealthLevel::Warning);
        assert!(format!("{danger:?}").contains("DaemonUnresponsive"));
    }

    #[test]
    fn a_tracker_is_comparable_frame_material() {
        let mut tracker = DaemonHealthTracker::default();
        tracker.observe(&sample(1_000));
        let clone = tracker;
        assert_eq!(clone, tracker);
        assert_ne!(clone, DaemonHealthTracker::default());
        assert!(format!("{tracker:?}").contains("observed"));
    }
}
