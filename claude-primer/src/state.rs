use crate::config;
use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The prime landed; a window opened at `ts`.
    Ok,
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
            Outcome::MissedTooStale => "missed: too stale",
            Outcome::SkippedNotScheduled => "skipped: not a scheduled weekday",
            Outcome::Error => "error",
            Outcome::DryRun => "dry-run",
        }
    }

    /// Whether this outcome actually opened a 5-hour window.
    pub fn opened_window(&self) -> bool {
        matches!(self, Outcome::Ok)
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
