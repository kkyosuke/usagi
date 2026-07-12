//! Scheduling a one-shot **wake**: a timed broadcast that types `continue` into
//! every running session agent at a chosen moment on the current day.
//!
//! The workspace `wake` command (the `:` command palette) schedules it, and the
//! home event loop checks each tick whether the moment has arrived — when it
//! has, it sends `continue` to every session with a live agent pane so a batch of
//! paused agents all resume at once. This module is the pure core: parsing the
//! absolute `-t hhmm` argument ([`parse_hhmm`]) and the relative `-i <dur>`
//! argument ([`parse_duration`]), and deciding when a schedule is due
//! ([`WakeSchedule`]), with the wall clock injected so all are unit-tested
//! without real time.

use chrono::{DateTime, Duration, Local, Timelike};

/// A one-shot wake scheduled for a specific instant on the current day.
///
/// Built by [`for_today`](Self::for_today) from the moment the command runs and
/// the requested `hh:mm`, so a time already past today is rejected up front
/// rather than firing on the next tick. Stored on the home screen state; the loop
/// asks [`is_due`](Self::is_due) each tick and fires once when it first returns
/// true.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeSchedule {
    /// The local instant at which the broadcast fires.
    at: DateTime<Local>,
}

impl WakeSchedule {
    /// Build a wake for `hour:minute` on `now`'s calendar day.
    ///
    /// `now` is the moment the command runs (the loop passes [`Local::now`]);
    /// the target keeps `now`'s date and time zone with the clock set to
    /// `hour:minute:00`. An out-of-range `hour`/`minute` (rejected by
    /// [`chrono::Timelike::with_hour`] / `with_minute`) or a target that is not in
    /// the future today is an `Err` with a human-readable reason, so `wake -t`
    /// never silently schedules a moment that has already gone by.
    pub fn for_today(now: DateTime<Local>, hour: u32, minute: u32) -> Result<Self, String> {
        let at = now
            .with_hour(hour)
            .and_then(|t| t.with_minute(minute))
            .and_then(|t| t.with_second(0))
            .and_then(|t| t.with_nanosecond(0))
            .ok_or_else(|| format!("{hour:02}:{minute:02} is not a valid time"))?;
        if at <= now {
            return Err(format!("{hour:02}:{minute:02} has already passed today"));
        }
        Ok(Self { at })
    }

    /// Build a wake `minutes` from `now` — the relative form behind `wake -i`.
    ///
    /// Unlike [`for_today`](Self::for_today) this is not clamped to the calendar
    /// day (a long-enough nudge may cross midnight) and needs no "already passed"
    /// check: a positive offset is always in the future. The caller
    /// ([`parse_duration`]) guarantees `minutes` is positive and within a day, so
    /// the target is a sensible same-day-ish instant.
    pub fn after(now: DateTime<Local>, minutes: i64) -> Self {
        Self {
            at: now + Duration::minutes(minutes),
        }
    }

    /// The local instant the broadcast fires at, for the confirmation message.
    pub fn at(&self) -> DateTime<Local> {
        self.at
    }

    /// Whether the scheduled moment has arrived by `now` (the loop's tick clock).
    pub fn is_due(&self, now: DateTime<Local>) -> bool {
        now >= self.at
    }
}

/// Parse the `hhmm` argument of `wake -t hhmm` into `(hour, minute)`.
///
/// Accepts a bare digit run (`1430`, or `930` for `9:30`) or a colon form
/// (`14:30`, `9:05`); surrounding whitespace is ignored. Returns a
/// human-readable `Err` for anything else — non-digits, the wrong number of
/// digits, or an hour/minute out of range (`00–23` / `00–59`).
pub fn parse_hhmm(raw: &str) -> Result<(u32, u32), String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("expected a time like 1430 or 14:30".to_string());
    }
    let (hh, mm) = match raw.split_once(':') {
        Some((hh, mm)) => (hh, mm),
        None => {
            if !raw.chars().all(|c| c.is_ascii_digit()) {
                return Err(format!(
                    "invalid time \"{raw}\": use digits like 1430 or 14:30"
                ));
            }
            match raw.len() {
                3 => (&raw[..1], &raw[1..]),
                4 => (&raw[..2], &raw[2..]),
                _ => return Err(format!("invalid time \"{raw}\": use hhmm like 1430")),
            }
        }
    };
    let hour: u32 = hh
        .parse()
        .map_err(|_| format!("invalid hour \"{hh}\": use a number like 14"))?;
    let minute: u32 = mm
        .parse()
        .map_err(|_| format!("invalid minute \"{mm}\": use a number like 30"))?;
    if hour > 23 {
        return Err(format!("invalid hour {hour}: must be 00–23"));
    }
    if minute > 59 {
        return Err(format!("invalid minute {minute}: must be 00–59"));
    }
    Ok((hour, minute))
}

/// The reason string shared by the segment guard and the total check, so a
/// too-long duration reads the same wherever it is rejected.
const DURATION_TOO_LONG: &str = "duration must be 24h or less";

/// Parse the `-i` argument of `wake -i <dur>` into a positive number of minutes.
///
/// Accepts `<number><unit>` segments with `h` (hours) and `m` (minutes),
/// combined in that order (`30m`, `2h`, `90m`, `1h30m`, `1h`); case and
/// surrounding whitespace are ignored, and a **bare number is read as minutes**
/// (`45` → 45m). Returns a human-readable `Err` for anything else — no digits, an
/// unknown unit, a trailing number with no unit (`1h30`), zero, or more than 24h
/// (a wake is a same-day nudge, not a multi-day timer).
pub fn parse_duration(raw: &str) -> Result<i64, String> {
    let raw = raw.trim().to_ascii_lowercase();
    let invalid = || format!("invalid duration \"{raw}\": use forms like 30m, 2h, or 1h30m");
    if raw.is_empty() {
        return Err(invalid());
    }
    // A bare number is the common "in N minutes" case.
    if raw.chars().all(|c| c.is_ascii_digit()) {
        return raw
            .parse::<i64>()
            .map_err(|_| invalid())
            .and_then(clamp_minutes);
    }
    // Otherwise sum `<number><unit>` segments left to right.
    let mut total: i64 = 0;
    let mut number = String::new();
    for c in raw.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let value = number.parse::<i64>().map_err(|_| invalid())?;
        number.clear();
        // Bound each segment up front so the running total can never overflow;
        // a segment over a day is itself too long.
        if value > 24 * 60 {
            return Err(DURATION_TOO_LONG.to_string());
        }
        match c {
            'h' => total += value * 60,
            'm' => total += value,
            _ => return Err(invalid()),
        }
    }
    // Trailing digits with no unit (e.g. `1h30`) are ambiguous — reject them.
    if !number.is_empty() {
        return Err(invalid());
    }
    clamp_minutes(total)
}

/// Reject a non-positive or over-a-day minute count, so `wake -i` never
/// schedules an instantaneous or multi-day wake.
fn clamp_minutes(minutes: i64) -> Result<i64, String> {
    if minutes <= 0 {
        return Err("duration must be greater than 0".to_string());
    }
    if minutes > 24 * 60 {
        return Err(DURATION_TOO_LONG.to_string());
    }
    Ok(minutes)
}

/// Render how long until `at` from `now` as a short human phrase for the wake
/// confirmation and `wake status` lines — `"in 30m"`, `"in 2h 5m"`, `"in 1h"`,
/// or `"in under a minute"` for a sub-minute wait. A non-positive delta (the
/// moment has arrived or already slipped by) reads `"now"`, so a status check the
/// instant a wake becomes due never shows a negative countdown.
pub fn humanize_until(now: DateTime<Local>, at: DateTime<Local>) -> String {
    let delta = at - now;
    if delta.num_seconds() <= 0 {
        return "now".to_string();
    }
    let total_minutes = delta.num_minutes();
    if total_minutes == 0 {
        return "in under a minute".to_string();
    }
    let (hours, minutes) = (total_minutes / 60, total_minutes % 60);
    match (hours, minutes) {
        (0, m) => format!("in {m}m"),
        (h, 0) => format!("in {h}h"),
        (h, m) => format!("in {h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A fixed local instant (14:00:00 on an arbitrary day) to schedule against.
    fn at_two_pm() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 7, 10, 14, 0, 0)
            .single()
            .unwrap()
    }

    #[test]
    fn parse_hhmm_accepts_four_digits() {
        assert_eq!(parse_hhmm("1430"), Ok((14, 30)));
        assert_eq!(parse_hhmm("0000"), Ok((0, 0)));
        assert_eq!(parse_hhmm("2359"), Ok((23, 59)));
    }

    #[test]
    fn parse_hhmm_accepts_three_digits_as_h_mm() {
        assert_eq!(parse_hhmm("930"), Ok((9, 30)));
        assert_eq!(parse_hhmm("005"), Ok((0, 5)));
    }

    #[test]
    fn parse_hhmm_accepts_a_colon_form_and_trims() {
        assert_eq!(parse_hhmm("14:30"), Ok((14, 30)));
        assert_eq!(parse_hhmm("  9:05 "), Ok((9, 5)));
    }

    #[test]
    fn parse_hhmm_rejects_empty_input() {
        assert!(parse_hhmm("   ").unwrap_err().contains("expected a time"));
    }

    #[test]
    fn parse_hhmm_rejects_non_digits() {
        assert!(parse_hhmm("14a0").unwrap_err().contains("use digits"));
    }

    #[test]
    fn parse_hhmm_rejects_the_wrong_digit_count() {
        assert!(parse_hhmm("14300").unwrap_err().contains("hhmm"));
        assert!(parse_hhmm("14").unwrap_err().contains("hhmm"));
    }

    #[test]
    fn parse_hhmm_rejects_a_non_numeric_colon_part() {
        assert!(parse_hhmm("aa:30").unwrap_err().contains("invalid hour"));
        assert!(parse_hhmm("14:bb").unwrap_err().contains("invalid minute"));
    }

    #[test]
    fn parse_hhmm_rejects_out_of_range_values() {
        assert!(parse_hhmm("2401").unwrap_err().contains("00–23"));
        assert!(parse_hhmm("1260").unwrap_err().contains("00–59"));
    }

    #[test]
    fn for_today_schedules_a_future_time_the_same_day() {
        let now = at_two_pm();
        let schedule = WakeSchedule::for_today(now, 14, 30).unwrap();
        let expected = Local
            .with_ymd_and_hms(2026, 7, 10, 14, 30, 0)
            .single()
            .unwrap();
        assert_eq!(schedule.at(), expected);
    }

    #[test]
    fn for_today_rejects_a_time_already_passed() {
        let now = at_two_pm();
        // 13:00 is earlier the same day.
        assert!(WakeSchedule::for_today(now, 13, 0)
            .unwrap_err()
            .contains("already passed"));
        // The current minute counts as passed (not in the future).
        assert!(WakeSchedule::for_today(now, 14, 0)
            .unwrap_err()
            .contains("already passed"));
    }

    #[test]
    fn for_today_rejects_an_out_of_range_clock() {
        let now = at_two_pm();
        assert!(WakeSchedule::for_today(now, 24, 0)
            .unwrap_err()
            .contains("not a valid time"));
        assert!(WakeSchedule::for_today(now, 14, 60)
            .unwrap_err()
            .contains("not a valid time"));
    }

    #[test]
    fn parse_duration_accepts_units_and_a_bare_number() {
        assert_eq!(parse_duration("30m"), Ok(30));
        assert_eq!(parse_duration("2h"), Ok(120));
        assert_eq!(parse_duration("90m"), Ok(90));
        assert_eq!(parse_duration("1h30m"), Ok(90));
        assert_eq!(parse_duration("1h"), Ok(60));
        // A bare number is minutes; case and whitespace are ignored.
        assert_eq!(parse_duration("45"), Ok(45));
        assert_eq!(parse_duration("  1H30M "), Ok(90));
        // The exact day boundary is allowed.
        assert_eq!(parse_duration("24h"), Ok(24 * 60));
        assert_eq!(parse_duration("1440m"), Ok(24 * 60));
    }

    #[test]
    fn parse_duration_rejects_bad_input() {
        assert!(parse_duration("").unwrap_err().contains("invalid duration"));
        assert!(parse_duration("   ")
            .unwrap_err()
            .contains("invalid duration"));
        // Unknown unit, and a trailing number with no unit.
        assert!(parse_duration("30s")
            .unwrap_err()
            .contains("invalid duration"));
        assert!(parse_duration("1h30")
            .unwrap_err()
            .contains("invalid duration"));
        // A leading unit with no number.
        assert!(parse_duration("h30m")
            .unwrap_err()
            .contains("invalid duration"));
        // Zero and over-a-day are rejected with their own reasons.
        assert!(parse_duration("0").unwrap_err().contains("greater than 0"));
        assert!(parse_duration("0m").unwrap_err().contains("greater than 0"));
        assert!(parse_duration("25h").unwrap_err().contains("24h or less"));
        assert!(parse_duration("1441m").unwrap_err().contains("24h or less"));
        // A single segment over a day is caught before it can overflow the total.
        assert!(parse_duration("2000m").unwrap_err().contains("24h or less"));
        // A number too big even to parse falls back to the format error.
        assert!(parse_duration("99999999999999999999")
            .unwrap_err()
            .contains("invalid duration"));
    }

    #[test]
    fn after_schedules_relative_to_now() {
        let now = at_two_pm();
        let schedule = WakeSchedule::after(now, 90);
        let expected = Local
            .with_ymd_and_hms(2026, 7, 10, 15, 30, 0)
            .single()
            .unwrap();
        assert_eq!(schedule.at(), expected);
        // A long-enough offset crosses into the next day, unlike `for_today`.
        let crosses = WakeSchedule::after(now, 11 * 60);
        assert_eq!(
            crosses.at(),
            Local
                .with_ymd_and_hms(2026, 7, 11, 1, 0, 0)
                .single()
                .unwrap()
        );
    }

    #[test]
    fn humanize_until_reads_as_a_short_countdown() {
        let now = at_two_pm();
        let at = |h, m, s| {
            Local
                .with_ymd_and_hms(2026, 7, 10, h, m, s)
                .single()
                .unwrap()
        };
        // Sub-minute waits collapse to a friendly phrase rather than "0m".
        assert_eq!(humanize_until(now, at(14, 0, 30)), "in under a minute");
        // Minutes only, hours only, and the combined form.
        assert_eq!(humanize_until(now, at(14, 30, 0)), "in 30m");
        assert_eq!(humanize_until(now, at(16, 0, 0)), "in 2h");
        assert_eq!(humanize_until(now, at(16, 5, 0)), "in 2h 5m");
        // The exact moment and a moment already gone both read "now" — never a
        // negative countdown.
        assert_eq!(humanize_until(now, at(14, 0, 0)), "now");
        assert_eq!(humanize_until(now, at(13, 0, 0)), "now");
    }

    #[test]
    fn is_due_flips_once_the_moment_arrives() {
        let now = at_two_pm();
        let schedule = WakeSchedule::for_today(now, 14, 30).unwrap();
        let before = Local
            .with_ymd_and_hms(2026, 7, 10, 14, 29, 59)
            .single()
            .unwrap();
        let at = Local
            .with_ymd_and_hms(2026, 7, 10, 14, 30, 0)
            .single()
            .unwrap();
        let after = Local
            .with_ymd_and_hms(2026, 7, 10, 14, 30, 1)
            .single()
            .unwrap();
        assert!(!schedule.is_due(before));
        assert!(schedule.is_due(at));
        assert!(schedule.is_due(after));
    }
}
