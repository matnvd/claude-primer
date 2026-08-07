//! Machine-readable state, for the menu bar app.
//!
//! This is the contract between the Rust CLI and `menubar/main.swift`. The Swift side
//! deliberately holds **no window logic of its own** — it renders what this emits.
//! Duplicating the 5-hour arithmetic over there would guarantee the two eventually
//! disagree, and this side is the one with tests.
//!
//! It reads local files only: no network, no tokens, and never the `claude` binary.
//! The menu bar polls this every 30s, so reaching the prime path here would open a
//! window on every refresh.

use crate::config::{Config, AGENT_LABEL, DAEMON_LABEL};
use crate::state::{self, Outcome};
use crate::{launchd, window};
use anyhow::Result;
use chrono::{DateTime, Local};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub generated_at: DateTime<Local>,
    pub window: WindowState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_prime: Option<Prime>,
    pub today: Today,
    /// The last few runs, newest last, regardless of which day they fell on — so the
    /// menu still has something to show just after midnight. Dry-runs are filtered out
    /// here rather than in the UI, keeping the decision in one place.
    pub recent: Vec<RunSummary>,
    pub upcoming: Vec<Prime>,
    pub units: Units,
    /// Severity is resolved here, once, so the Swift side never re-derives it and the
    /// two surfaces cannot disagree about what counts as a problem.
    pub health: Health,
    pub paths: Paths,
}

#[derive(Debug, Serialize)]
pub struct WindowState {
    pub open: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Local>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<DateTime<Local>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_secs: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct Prime {
    pub at: DateTime<Local>,
    pub anchor: String,
}

#[derive(Debug, Serialize)]
pub struct Today {
    pub scheduled: bool,
    pub expected: usize,
    pub done: usize,
    pub had_stale_miss: bool,
    /// A prime ran but opened nothing — quota spent, schedule unmoved.
    pub had_wasted: bool,
}

#[derive(Debug, Serialize)]
pub struct RunSummary {
    pub ts: DateTime<Local>,
    pub anchor: String,
    /// The `Outcome` enum's own snake_case serialization, e.g. `missed_too_stale`.
    pub outcome: Outcome,
    /// Human-readable form of the same thing, so the GUI needn't map the enum.
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct Units {
    pub agent: UnitState,
    pub daemon: UnitState,
}

#[derive(Debug, Serialize)]
pub struct UnitState {
    pub loaded: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_exit: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Health {
    Ok,
    /// A prime was skipped as stale — the schedule is intact but a window was lost.
    Warn,
    /// The agent is not loaded, or a prime failed outright. Nothing will fire.
    Error,
}

#[derive(Debug, Serialize)]
pub struct Paths {
    pub config: String,
    pub runs_log: String,
}

/// The raw state everything else is derived from. Separate from [`Snapshot`] because
/// this is internal shape; `Snapshot` is the JSON contract the menu bar renders.
pub struct Readout {
    pub window_remaining: Option<chrono::Duration>,
    pub primes_done: usize,
    pub primes_expected: usize,
    pub had_stale_miss: bool,
    /// A prime ran but opened nothing, because a window was already in flight.
    pub had_wasted: bool,
    pub agent_healthy: bool,
    pub scheduled_today: bool,
}

pub fn gather(cfg: &Config, now: DateTime<Local>) -> Result<Readout> {
    let window_remaining = state::last_window_start()?.and_then(|start| {
        let ends = start + window::window_len();
        (ends > now).then(|| ends - now)
    });

    let todays = state::runs_on_date(now.date_naive())?;
    // Counts windows opened, not calls that succeeded — a prime landing inside an open
    // window returns fine but achieves nothing.
    let primes_done = todays.iter().filter(|r| r.outcome.opened_window()).count();
    let had_stale_miss = todays.iter().any(|r| r.outcome == Outcome::MissedTooStale);
    let had_wasted = todays.iter().any(|r| r.outcome.wasted());

    // Today's own anchor count, which differs from the base set on a day with a
    // per-day schedule.
    let todays_anchors = cfg.anchors_for(cfg.today()?)?;

    let unit = launchd::unit_status(AGENT_LABEL);

    Ok(Readout {
        window_remaining,
        primes_done,
        primes_expected: todays_anchors.len(),
        had_stale_miss,
        had_wasted,
        agent_healthy: unit.loaded && unit.last_exit.map(|e| e == 0).unwrap_or(true),
        scheduled_today: !todays_anchors.is_empty(),
    })
}

/// How many past runs the menu shows.
const RECENT_RUNS: usize = 3;

pub fn build(cfg: &Config, now: DateTime<Local>) -> Result<Snapshot> {
    let snap = gather(cfg, now)?;
    let started_at = state::last_window_start()?;

    // `window_remaining` is derived from `last_window_start`, so remaining-without-a-
    // start cannot occur; treating it as closed keeps that assumption from becoming a
    // panic if the derivation ever changes.
    let window_state = WindowState {
        open: started_at.is_some() && snap.window_remaining.is_some(),
        started_at,
        ends_at: started_at.map(|s| s + window::window_len()),
        remaining_secs: started_at
            .and(snap.window_remaining)
            .map(|rem| rem.num_seconds()),
    };

    let upcoming: Vec<Prime> = window::upcoming(cfg, 14)?
        .into_iter()
        .take(5)
        .map(|(_, a, at)| Prime { at, anchor: a.label() })
        .collect();

    let summarize = |r: crate::state::RunRecord| RunSummary {
        ts: r.ts,
        anchor: r.anchor,
        outcome: r.outcome,
        label: r.outcome.label().to_string(),
        cost_usd: r.cost_usd,
    };

    // Today's runs feed the health check only; they are no longer serialized, since
    // `recent` is what the menu renders.
    let runs: Vec<RunSummary> =
        state::runs_on_date(now.date_naive())?.into_iter().map(summarize).collect();

    let mut past: Vec<_> = state::read_runs()?
        .into_iter()
        .filter(|r| r.outcome != Outcome::DryRun)
        .collect();
    let recent: Vec<RunSummary> =
        past.split_off(past.len().saturating_sub(RECENT_RUNS)).into_iter().map(summarize).collect();

    let agent = unit_state(AGENT_LABEL);
    let daemon = unit_state(DAEMON_LABEL);

    let health = health_of(&snap, &runs);

    Ok(Snapshot {
        generated_at: now,
        window: window_state,
        next_prime: upcoming.first().map(|p| Prime { at: p.at, anchor: p.anchor.clone() }),
        today: Today {
            scheduled: snap.scheduled_today,
            expected: snap.primes_expected,
            done: snap.primes_done,
            had_stale_miss: snap.had_stale_miss,
            had_wasted: snap.had_wasted,
        },
        recent,
        upcoming,
        units: Units { agent, daemon },
        health,
        paths: Paths {
            config: Config::path()?.display().to_string(),
            runs_log: crate::config::runs_log()?.display().to_string(),
        },
    })
}

/// Severity, resolved once so no surface can disagree with another.
///
/// An agent that isn't loaded means *nothing* fires, so it outranks a stale miss or a
/// wasted prime, each of which costs only a single window.
fn health_of(snap: &Readout, runs: &[RunSummary]) -> Health {
    if !snap.agent_healthy {
        Health::Error
    } else if snap.had_stale_miss
        || snap.had_wasted
        || runs.iter().any(|r| r.outcome == Outcome::Error)
    {
        Health::Warn
    } else {
        Health::Ok
    }
}

fn unit_state(label: &str) -> UnitState {
    let u = launchd::unit_status(label);
    UnitState { loaded: u.loaded, last_exit: u.last_exit }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readout(agent_healthy: bool, stale: bool, wasted: bool) -> Readout {
        Readout {
            window_remaining: None,
            primes_done: 3,
            primes_expected: 3,
            had_stale_miss: stale,
            had_wasted: wasted,
            agent_healthy,
            scheduled_today: true,
        }
    }

    fn run_with(outcome: Outcome) -> RunSummary {
        RunSummary {
            ts: Local::now(),
            anchor: "10:30".into(),
            outcome,
            label: outcome.label().to_string(),
            cost_usd: None,
        }
    }

    #[test]
    fn a_clean_day_is_ok() {
        assert_eq!(health_of(&readout(true, false, false), &[]), Health::Ok);
    }

    #[test]
    fn a_stale_miss_or_a_wasted_prime_warns() {
        assert_eq!(health_of(&readout(true, true, false), &[]), Health::Warn);
        assert_eq!(health_of(&readout(true, false, true), &[]), Health::Warn);
    }

    #[test]
    fn a_failed_prime_warns() {
        assert_eq!(health_of(&readout(true, false, false), &[run_with(Outcome::Error)]), Health::Warn);
    }

    #[test]
    fn an_unloaded_agent_outranks_everything_else() {
        // Nothing fires at all, so it must not be softened to a warning by a day that
        // also happens to have a stale miss.
        assert_eq!(health_of(&readout(false, true, true), &[run_with(Outcome::Error)]), Health::Error);
    }

    #[test]
    fn health_is_serialized_lowercase() {
        assert_eq!(serde_json::to_string(&Health::Ok).unwrap(), r#""ok""#);
        assert_eq!(serde_json::to_string(&Health::Warn).unwrap(), r#""warn""#);
        assert_eq!(serde_json::to_string(&Health::Error).unwrap(), r#""error""#);
    }

    #[test]
    fn a_closed_window_still_reports_its_start() {
        // The GUI shows "ended 14:02" rather than nothing at all.
        let w = WindowState {
            open: false,
            started_at: Some(Local::now()),
            ends_at: Some(Local::now()),
            remaining_secs: None,
        };
        let v: serde_json::Value = serde_json::to_value(&w).unwrap();
        assert_eq!(v["open"], false);
        assert!(v.get("started_at").is_some());
        assert!(v.get("remaining_secs").is_none(), "absent, not null");
    }

    #[test]
    fn run_summaries_carry_both_enum_and_label() {
        // The enum is for logic, the label for display — so Swift maps neither.
        let r = RunSummary {
            ts: Local::now(),
            anchor: "05:30".into(),
            outcome: Outcome::MissedTooStale,
            label: Outcome::MissedTooStale.label().to_string(),
            cost_usd: None,
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["outcome"], "missed_too_stale");
        assert_eq!(v["label"], "missed: too stale");
    }

    #[test]
    fn this_module_never_invokes_claude() {
        // Same guarantee as the status line: the menu bar polls this every 30s, so
        // reaching the prime path here would open a window on every refresh.
        let full = include_str!("snapshot.rs");
        let impl_src = full.split("#[cfg(test)]").next().unwrap();
        assert!(!impl_src.contains("claude_bin"), "must not read the claude binary path");
        assert!(!impl_src.contains("crate::prime"), "must not reach the prime path");
        assert!(!impl_src.contains("reqwest"), "must not use an HTTP client");
        assert!(!impl_src.contains("TcpStream"), "must not open a socket");
    }
}
