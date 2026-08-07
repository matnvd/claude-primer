use crate::config;
use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The prime landed and opened a new 5-hour window at `ts`.
    Ok,
    /// The prime landed, but a window was already open, so it opened nothing — it only
    /// spent quota from the window already running. Anchors spaced exactly 5h apart
    /// make this easy to hit: a few seconds of drift is enough to land inside the
    /// previous window. Deliberately not `Ok`, because counting it as a window start
    /// would reset the countdown to 5h when the real window ends sooner.
    OkWindowAlreadyOpen,
    /// The anchor passed while the Mac was off or asleep and launchd caught up late.
    /// Firing would have opened a misaligned window, so nothing was spent.
    MissedTooStale,
    /// Not a scheduled weekday.
    SkippedNotScheduled,
    /// The `claude` call itself failed.
    Error,
    /// `--dry-run`: the command was composed but never executed.
    DryRun,
}

impl Outcome {
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::OkWindowAlreadyOpen => "wasted: window already open",
            Outcome::MissedTooStale => "missed: too stale",
            Outcome::SkippedNotScheduled => "skipped: not a scheduled weekday",
            Outcome::Error => "error",
            Outcome::DryRun => "dry-run",
        }
    }

    /// Whether this outcome actually opened a 5-hour window.
    ///
    /// This is what `last_window_start` filters on, so anything returning `true` here
    /// becomes the anchor for every countdown the tool reports. A successful `claude`
    /// call is *not* sufficient — the window has to have actually started.
    pub fn opened_window(&self) -> bool {
        matches!(self, Outcome::Ok)
    }

    /// Quota was spent for no scheduling benefit. Worth surfacing: it means an anchor
    /// is misplaced relative to the window it was meant to open.
    pub fn wasted(&self) -> bool {
        matches!(self, Outcome::OkWindowAlreadyOpen)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub ts: DateTime<Local>,
    pub anchor: String,
    pub outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled_for: Option<DateTime<Local>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub late_by_minutes: Option<i64>,
    /// When a window was already running at prime time, when it was due to end. Set
    /// only on `OkWindowAlreadyOpen`, so the waste is diagnosable after the fact.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_open_until: Option<DateTime<Local>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RunRecord {
    pub fn new(anchor: &str, outcome: Outcome) -> Self {
        Self {
            ts: Local::now(),
            anchor: anchor.to_string(),
            outcome,
            scheduled_for: None,
            late_by_minutes: None,
            window_open_until: None,
            cost_usd: None,
            session_id: None,
            duration_ms: None,
            error: None,
        }
    }
}

pub fn append_run(rec: &RunRecord) -> Result<()> {
    let path = config::runs_log()?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(f, "{}", serde_json::to_string(rec)?)?;
    Ok(())
}

pub fn read_runs() -> Result<Vec<RunRecord>> {
    let path = config::runs_log()?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(path)?;
    // Tolerate a partially-written trailing line rather than failing the status line.
    Ok(raw.lines().filter_map(|l| serde_json::from_str(l).ok()).collect())
}

/// The most recent run that actually opened a window.
pub fn last_window_start() -> Result<Option<DateTime<Local>>> {
    Ok(read_runs()?
        .into_iter()
        .filter(|r| r.outcome.opened_window())
        .map(|r| r.ts)
        .max())
}

pub fn runs_on_date(date: chrono::NaiveDate) -> Result<Vec<RunRecord>> {
    Ok(read_runs()?
        .into_iter()
        .filter(|r| r.ts.date_naive() == date)
        .collect())
}

/// One-time `pmset schedule wake` events we armed, recorded so they can be cancelled
/// by exact match later. `pmset schedule cancelall` is never used — it would also
/// destroy the system's own calendar-alarm and analytics wakes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WakeLedger {
    pub armed: Vec<ArmedWake>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_time_local: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat_weekdays: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArmedWake {
    /// Exactly the string handed to pmset: "MM/dd/yy HH:mm:ss", system-local.
    pub datetime: String,
    pub for_anchor: String,
}

impl WakeLedger {
    pub fn load() -> Result<Self> {
        let path = config::wake_ledger()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?).unwrap_or_default())
    }

    pub fn save(&self) -> Result<()> {
        let path = config::wake_ledger()?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ok_opens_a_window() {
        assert!(Outcome::Ok.opened_window());
        assert!(!Outcome::MissedTooStale.opened_window());
        assert!(!Outcome::DryRun.opened_window());
        assert!(!Outcome::Error.opened_window());
        assert!(!Outcome::SkippedNotScheduled.opened_window());
    }

    #[test]
    fn a_prime_into_an_open_window_does_not_count_as_opening_one() {
        // The `claude` call succeeded, but nothing opened. Treating this as Ok would
        // make `last_window_start` return the prime's timestamp and reset every
        // countdown to a full 5 hours while the real window ends sooner.
        assert!(!Outcome::OkWindowAlreadyOpen.opened_window());
        assert!(Outcome::OkWindowAlreadyOpen.wasted());
        assert!(!Outcome::Ok.wasted());
    }

    #[test]
    fn a_wasted_prime_is_not_picked_as_the_window_start() {
        // Guards the interaction directly: last_window_start filters on opened_window.
        let opening = RunRecord::new("05:30", Outcome::Ok);
        let wasted = RunRecord::new("10:30", Outcome::OkWindowAlreadyOpen);
        let picked: Vec<_> = [opening.clone(), wasted]
            .into_iter()
            .filter(|r| r.outcome.opened_window())
            .map(|r| r.anchor)
            .collect();
        assert_eq!(picked, vec!["05:30"]);
    }

    #[test]
    fn the_open_window_end_is_recorded_for_diagnosis() {
        let mut rec = RunRecord::new("10:30", Outcome::OkWindowAlreadyOpen);
        rec.window_open_until = Some(Local::now());
        let line = serde_json::to_string(&rec).unwrap();
        assert!(line.contains("window_open_until"));
        assert!(line.contains("ok_window_already_open"));
    }

    #[test]
    fn records_round_trip_through_jsonl() {
        let mut rec = RunRecord::new("05:30", Outcome::Ok);
        rec.cost_usd = Some(0.0012);
        rec.session_id = Some("abc".into());
        let line = serde_json::to_string(&rec).unwrap();
        let back: RunRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.anchor, "05:30");
        assert_eq!(back.outcome, Outcome::Ok);
        assert_eq!(back.cost_usd, Some(0.0012));
    }

    #[test]
    fn absent_optional_fields_are_omitted() {
        let line = serde_json::to_string(&RunRecord::new("10:30", Outcome::MissedTooStale)).unwrap();
        assert!(!line.contains("cost_usd"));
        assert!(line.contains("missed_too_stale"));
    }

    #[test]
    fn ledger_defaults_when_absent() {
        let l = WakeLedger::default();
        assert!(l.armed.is_empty());
        assert!(l.repeat_time_local.is_none());
    }
}
