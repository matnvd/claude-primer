//! Machine-readable state, for the menu bar app.
//!
//! This is the contract between the Rust CLI and `menubar/main.swift`. The Swift side
//! deliberately holds **no window logic of its own** — it renders what this emits.
//! Duplicating the 5-hour arithmetic over there would guarantee the two eventually
//! disagree, and this side is the one with tests.
//!
//! Like [`crate::statusline`], this reads local files only: no network, no tokens, and
//! never the `claude` binary.

use crate::config::{Config, AGENT_LABEL, DAEMON_LABEL};
use crate::state::{self, Outcome};
use crate::{launchd, statusline, window};
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
    pub runs: Vec<RunSummary>,
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

pub fn build(cfg: &Config, now: DateTime<Local>) -> Result<Snapshot> {
    let snap = statusline::snapshot(cfg, now)?;
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

    let runs: Vec<RunSummary> = state::runs_on_date(now.date_naive())?
        .into_iter()
        .map(|r| RunSummary {
            ts: r.ts,
            anchor: r.anchor,
            outcome: r.outcome,
            label: r.outcome.label().to_string(),
            cost_usd: r.cost_usd,
        })
        .collect();

    let agent = unit_state(AGENT_LABEL);
    let daemon = unit_state(DAEMON_LABEL);

    // An agent that isn't loaded means nothing fires at all, so it outranks a stale
    // miss, which only means one window was lost.
    let health = if !snap.agent_healthy {
        Health::Error
    } else if snap.had_stale_miss || runs.iter().any(|r| r.outcome == Outcome::Error) {
        Health::Warn
    } else {
        Health::Ok
    };

    Ok(Snapshot {
        generated_at: now,
        window: window_state,
        next_prime: upcoming.first().map(|p| Prime { at: p.at, anchor: p.anchor.clone() }),
        today: Today {
            scheduled: snap.scheduled_today,
            expected: snap.primes_expected,
            done: snap.primes_done,
            had_stale_miss: snap.had_stale_miss,
            runs,
        },
        upcoming,
        units: Units { agent, daemon },
        health,
        paths: Paths {
            config: Config::path()?.display().to_string(),
            runs_log: crate::config::runs_log()?.display().to_string(),
        },
    })
}

fn unit_state(label: &str) -> UnitState {
    let u = launchd::unit_status(label);
    UnitState { loaded: u.loaded, last_exit: u.last_exit }
}

#[cfg(test)]
mod tests {
    use super::*;

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
