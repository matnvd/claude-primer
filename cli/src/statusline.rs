//! One-line readout for Claude Code's status line.
//!
//! **This module must never invoke `claude` or touch the network.** Claude Code runs
//! the status line as a local subprocess on a refresh timer — reading window state by
//! shelling out to `/usage` would cost tokens and, far worse, would start a new
//! 5-hour window on every refresh, destroying the very thing this tool controls.
//!
//! Everything here is local file I/O and date arithmetic.

use crate::config::{Config, AGENT_LABEL};
use crate::launchd;
use crate::state::{self, Outcome};
use crate::window;
use anyhow::Result;
use chrono::{DateTime, Duration, Local};

pub struct Snapshot {
    pub window_remaining: Option<Duration>,
    pub next_prime: Option<DateTime<Local>>,
    pub primes_done: usize,
    pub primes_expected: usize,
    pub had_stale_miss: bool,
    /// A prime ran but opened nothing, because a window was already in flight.
    pub had_wasted: bool,
    pub agent_healthy: bool,
    pub scheduled_today: bool,
}

pub fn snapshot(cfg: &Config, now: DateTime<Local>) -> Result<Snapshot> {
    let window_remaining = state::last_window_start()?.and_then(|start| {
        let ends = start + window::window_len();
        (ends > now).then(|| ends - now)
    });

    let next_prime = window::upcoming(cfg, 14)?.first().map(|(_, _, dt)| *dt);

    let today = now.date_naive();
    let todays = state::runs_on_date(today)?;
    // Counts windows opened, not calls that succeeded — a prime landing inside an open
    // window returns fine but achieves nothing.
    let primes_done = todays.iter().filter(|r| r.outcome.opened_window()).count();
    let had_stale_miss = todays.iter().any(|r| r.outcome == Outcome::MissedTooStale);
    let had_wasted = todays.iter().any(|r| r.outcome.wasted());

    // Today's own anchor count, which differs from the base set on a day with a
    // per-day schedule.
    let todays_anchors = cfg.anchors_for(cfg.today()?)?;
    let scheduled_today = !todays_anchors.is_empty();
    let primes_expected = todays_anchors.len();

    let unit = launchd::unit_status(AGENT_LABEL);
    let agent_healthy = unit.loaded && unit.last_exit.map(|e| e == 0).unwrap_or(true);

    Ok(Snapshot {
        window_remaining,
        next_prime,
        primes_done,
        primes_expected,
        had_stale_miss,
        had_wasted,
        agent_healthy,
        scheduled_today,
    })
}

pub fn render(s: &Snapshot) -> String {
    let mut parts: Vec<String> = Vec::new();

    match s.window_remaining {
        Some(d) => parts.push(format!("⏱ {} left in window", window::fmt_hm(d))),
        None => parts.push("⏱ no window open".to_string()),
    }

    if let Some(next) = s.next_prime {
        parts.push(format!("next prime {}", next.format("%H:%M")));
    }

    let mark = if !s.agent_healthy {
        "✗"
    } else if s.had_stale_miss || s.had_wasted {
        "⚠"
    } else if !s.scheduled_today {
        "—"
    } else {
        "✓"
    };

    if s.scheduled_today {
        parts.push(format!("{mark} {}/{} today", s.primes_done, s.primes_expected));
    } else {
        parts.push(format!("{mark} off today"));
    }

    parts.join(" · ")
}

/// Reads and discards Claude Code's session JSON on stdin. The status line contract
/// pipes it in; we don't need it, but leaving it unread can break the pipe.
pub fn drain_stdin() {
    use std::io::Read;
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Snapshot {
        Snapshot {
            window_remaining: Some(Duration::minutes(134)),
            next_prime: None,
            primes_done: 3,
            primes_expected: 3,
            had_stale_miss: false,
            had_wasted: false,
            agent_healthy: true,
            scheduled_today: true,
        }
    }

    #[test]
    fn healthy_day_renders_a_tick() {
        let out = render(&base());
        assert!(out.contains("2h14m left in window"));
        assert!(out.contains("✓ 3/3 today"));
    }

    #[test]
    fn a_stale_miss_downgrades_to_a_warning() {
        let s = Snapshot { had_stale_miss: true, primes_done: 2, ..base() };
        assert!(render(&s).contains("⚠ 2/3 today"));
    }

    #[test]
    fn an_unloaded_agent_outranks_a_stale_miss() {
        let s = Snapshot { agent_healthy: false, had_stale_miss: true, ..base() };
        assert!(render(&s).contains("✗"));
    }

    #[test]
    fn a_non_scheduled_day_shows_a_dash() {
        let s = Snapshot { scheduled_today: false, primes_expected: 0, ..base() };
        let out = render(&s);
        assert!(out.contains("— off today"));
        assert!(!out.contains("0/0"));
    }

    #[test]
    fn an_expired_window_says_so() {
        let s = Snapshot { window_remaining: None, ..base() };
        assert!(render(&s).contains("no window open"));
    }

    #[test]
    fn next_prime_is_included_when_known() {
        use chrono::TimeZone;
        let s = Snapshot {
            next_prime: Some(Local.with_ymd_and_hms(2026, 8, 6, 15, 30, 0).unwrap()),
            ..base()
        };
        assert!(render(&s).contains("next prime 15:30"));
    }

    #[test]
    fn this_module_never_invokes_claude() {
        // A status line that primed a window on every refresh would defeat the whole
        // tool. It may spawn cheap local tools (launchd::unit_status shells out to
        // launchctl), but it must never reach the configured claude binary or the
        // prime path. Literals are split so this assertion doesn't match itself.
        // Scan only the implementation, not this test block, or the assertions below
        // match their own text.
        let full = include_str!("statusline.rs");
        let impl_src = full.split("#[cfg(test)]").next().unwrap();

        assert!(!impl_src.contains("claude_bin"), "must not read the claude binary path");
        assert!(!impl_src.contains("crate::prime"), "must not reach the prime path");
        assert!(!impl_src.contains("reqwest"), "must not use an HTTP client");
        assert!(!impl_src.contains("TcpStream"), "must not open a socket");
    }
}
