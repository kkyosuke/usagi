//! Scheduling a one-shot **wake**: a timed broadcast that types `continue` into
//! every running session agent at a chosen moment on the current day.
//!
//! The workspace `wake` command (the `:` command palette) schedules it, and the
//! home event loop checks each tick whether the moment has arrived — when it
//! has, it sends `continue` to every session with a live agent pane so a batch of
//! paused agents all resume at once. This module is the pure core: parsing the
//! `-t hhmm` argument ([`parse_hhmm`]) and deciding when a schedule is due
//! ([`WakeSchedule`]), with the wall clock injected so both are unit-tested
//! without real time.

use chrono::{DateTime, Local, Timelike};

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
