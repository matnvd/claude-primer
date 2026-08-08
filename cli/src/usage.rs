//! Real usage state, read from Claude Code's own `/usage` command.
//!
//! `/usage` is answered client-side, so it costs **nothing**: measured at
//! `total_cost_usd: 0`, `input_tokens: 0`, `num_turns: 0`, and repeated calls leave the
//! window's reset time unchanged. That is what makes it safe to call from a status
//! surface, unlike an ordinary prime.
//!
//! This is the only source of *truth* the tool has. Everything else — `last_window_start`
//! and the countdowns derived from it — infers the window from this tool's own primes,
//! and is wrong whenever a window was opened somewhere it cannot see (claude.ai, another
//! machine, an interactive session).
//!
//! The catch: the numbers arrive as human-readable prose in the `result` field, not a
//! stable contract. Parsing is therefore deliberately forgiving, and every failure
//! degrades to `None` so callers fall back to the estimate rather than breaking.

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone};
use serde::Serialize;
use std::process::Command;

/// What `/usage` reports. Percentages are of the plan's allowance, server-side, and
/// include usage from every device — unlike the "what's contributing" breakdown further
/// down that output, which is explicitly local-only.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Usage {
    pub session_pct: Option<u8>,
    pub session_resets_at: Option<DateTime<Local>>,
    pub week_pct: Option<u8>,
    pub week_resets_at: Option<DateTime<Local>>,
}

impl Usage {
    fn is_empty(&self) -> bool {
        self.session_pct.is_none() && self.week_pct.is_none()
    }
}

/// Ask Claude Code for the current usage. `None` on any failure, so a status surface
/// keeps working when the format changes or the binary is unreachable.
pub fn fetch(claude_bin: &str) -> Option<Usage> {
    // Pin the working directory, exactly as a prime does. Without it this inherits the
    // caller's cwd — which for the menu bar app is `/` — and Claude Code then runs its
    // auto-discovery for hooks, plugins, MCP servers and CLAUDE.md from the filesystem
    // root. That walk reaches ~/Pictures and ~/Music, and since macOS attributes a
    // child's file access to the responsible parent, the *app* was the one prompting
    // for Photos and Music access.
    let cwd = crate::config::prime_cwd().ok()?;
    std::fs::create_dir_all(&cwd).ok()?;

    let out = Command::new(claude_bin)
        .args([
            "-p",
            "/usage",
            "--output-format",
            "json",
            // Nothing here needs MCP servers; loading them is latency and more file access.
            "--strict-mcp-config",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
        ])
        // Same suppression the primes use: no telemetry, no cache writes.
        .envs(crate::prime::build_env())
        .current_dir(&cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let text = v.get("result")?.as_str()?;
    parse(text, Local::now())
}

/// Pull the two lines that matter out of the prose.
///
/// ```text
/// Current session: 38% used · resets Aug 8 at 5:29pm (Atlantic/Bermuda)
/// Current week (all models): 66% used · resets Aug 10 at 5:59am (Atlantic/Bermuda)
/// ```
///
/// `now` is passed in rather than read here so the year can be inferred and the whole
/// thing stays testable.
pub fn parse(text: &str, now: DateTime<Local>) -> Option<Usage> {
    let mut u = Usage {
        session_pct: None,
        session_resets_at: None,
        week_pct: None,
        week_resets_at: None,
    };

    for line in text.lines() {
        let l = line.trim();
        // "Current week (all models):" must be tested first — it also starts with
        // "Current w", and matching "Current session" loosely would miss it entirely.
        let is_week = l.starts_with("Current week");
        let is_session = l.starts_with("Current session");
        if !is_week && !is_session {
            continue;
        }
        let pct = percent_before(l, "% used");
        let at = l.split("resets").nth(1).and_then(|rest| parse_reset(rest, now));
        if is_week {
            u.week_pct = pct;
            u.week_resets_at = at;
        } else {
            u.session_pct = pct;
            u.session_resets_at = at;
        }
    }

    (!u.is_empty()).then_some(u)
}

/// The integer immediately preceding `marker`, e.g. `38` from "… 38% used".
fn percent_before(line: &str, marker: &str) -> Option<u8> {
    let idx = line.find(marker)?;
    let digits: String = line[..idx]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse().ok()
}

/// Parse " Aug 8 at 5:29pm (Atlantic/Bermuda)" into an instant.
///
/// The year is absent, so it is inferred from `now`: a date that lands more than a few
/// days in the past is next year's, which keeps a reset just after New Year from being
/// read as eleven months ago. The trailing timezone is ignored — Claude Code prints the
/// machine's own zone, which is what `Local` already is.
fn parse_reset(rest: &str, now: DateTime<Local>) -> Option<DateTime<Local>> {
    let cleaned = rest.split('(').next()?.trim();
    let (date_part, time_part) = cleaned.split_once(" at ")?;

    let mut it = date_part.split_whitespace();
    let month = month_from_abbrev(it.next()?)?;
    let day: u32 = it.next()?.parse().ok()?;

    let t = time_part.trim().to_ascii_lowercase();
    let (clock, pm) = if let Some(s) = t.strip_suffix("pm") {
        (s, true)
    } else {
        (t.strip_suffix("am")?, false)
    };
    // On the hour Claude Code drops the minutes entirely — "6am", not "6:00am" — so a
    // parser that insists on a colon silently loses the reset time.
    let (h, m) = match clock.split_once(':') {
        Some((h, m)) => (h, m),
        None => (clock, "0"),
    };
    let mut hour: u32 = h.trim().parse().ok()?;
    let minute: u32 = m.trim().parse().ok()?;
    if pm && hour != 12 {
        hour += 12;
    } else if !pm && hour == 12 {
        hour = 0;
    }

    let build = |year: i32| -> Option<NaiveDateTime> {
        NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)
    };
    let naive = build(now.year())?;
    let naive = if (now.naive_local() - naive).num_days() > 3 {
        build(now.year() + 1)?
    } else {
        naive
    };
    Local.from_local_datetime(&naive).earliest()
}

fn month_from_abbrev(s: &str) -> Option<u32> {
    let m = match s.trim().to_ascii_lowercase().as_str() {
        "jan" => 1, "feb" => 2, "mar" => 3, "apr" => 4, "may" => 5, "jun" => 6,
        "jul" => 7, "aug" => 8, "sep" => 9, "oct" => 10, "nov" => 11, "dec" => 12,
        _ => return None,
    };
    Some(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "You are currently using your subscription to power your Claude Code usage\n\nCurrent session: 38% used · resets Aug 8 at 5:29pm (Atlantic/Bermuda)\nCurrent week (all models): 66% used · resets Aug 10 at 5:59am (Atlantic/Bermuda)\n\nWhat's contributing to your limits usage?\nApproximate, based on local sessions on this machine — does not include other devices or claude.ai.";

    fn aug8() -> DateTime<Local> {
        Local.with_ymd_and_hms(2026, 8, 8, 16, 0, 0).unwrap()
    }

    #[test]
    fn the_call_is_confined_to_the_empty_directory() {
        // Inheriting the caller's cwd let Claude Code's discovery walk start wherever
        // the parent happened to be — from `/` for the menu bar app, which reached the
        // protected media folders and made the app prompt for Photos and Music.
        let full = include_str!("usage.rs");
        let impl_src = full.split("#[cfg(test)]").next().unwrap();
        assert!(impl_src.contains("current_dir"), "must pin a working directory");
        assert!(impl_src.contains("prime_cwd"), "and it must be the dedicated empty one");
    }

    #[test]
    fn this_module_only_ever_asks_for_usage() {
        // The one thing that would make this unsafe to poll: passing a real prompt
        // instead of `/usage`, which would spend tokens and open a window on every
        // refresh of the menu bar.
        let full = include_str!("usage.rs");
        let impl_src = full.split("#[cfg(test)]").next().unwrap();
        let call = impl_src.split("fn fetch").nth(1).unwrap().split("fn ").next().unwrap();
        assert!(call.contains(r#""/usage""#), "the only prompt may be /usage");
        assert!(!call.contains("build_args"), "must not compose a priming call");
        assert!(!call.contains("prime::run"), "must not reach the prime path");
    }

    #[test]
    fn parses_the_real_output() {
        let u = parse(REAL, aug8()).expect("should parse");
        assert_eq!(u.session_pct, Some(38));
        assert_eq!(u.week_pct, Some(66));
        let s = u.session_resets_at.expect("session reset");
        assert_eq!(s.format("%Y-%m-%d %H:%M").to_string(), "2026-08-08 17:29");
        let w = u.week_resets_at.expect("week reset");
        assert_eq!(w.format("%Y-%m-%d %H:%M").to_string(), "2026-08-10 05:59");
    }

    #[test]
    fn the_week_line_is_not_swallowed_by_the_session_match() {
        // Both begin "Current ", and "Current week (all models):" carries a parenthetical
        // the session line lacks. Matching loosely drops one of them silently.
        let u = parse(REAL, aug8()).unwrap();
        assert!(u.session_pct.is_some() && u.week_pct.is_some());
        assert_ne!(u.session_pct, u.week_pct);
    }

    #[test]
    fn a_time_on_the_hour_has_no_minutes() {
        // Observed live: "resets Aug 10 at 6am (Atlantic/Bermuda)". Requiring a colon
        // dropped the weekly reset time while still reporting the percentage, so the
        // failure was invisible.
        let u = parse("Current week (all models): 67% used · resets Aug 10 at 6am (X)", aug8()).unwrap();
        assert_eq!(
            u.week_resets_at.expect("6am must parse").format("%Y-%m-%d %H:%M").to_string(),
            "2026-08-10 06:00"
        );
    }

    #[test]
    fn midnight_and_noon_convert_correctly() {
        let noon = parse("Current session: 5% used · resets Aug 8 at 12:00pm (X)", aug8()).unwrap();
        assert_eq!(noon.session_resets_at.unwrap().format("%H:%M").to_string(), "12:00");
        let midnight = parse("Current session: 5% used · resets Aug 9 at 12:30am (X)", aug8()).unwrap();
        assert_eq!(midnight.session_resets_at.unwrap().format("%H:%M").to_string(), "00:30");
    }

    #[test]
    fn a_reset_early_next_year_is_not_read_as_months_ago() {
        // On 31 Dec, "resets Jan 1" means tomorrow, not eleven months back.
        let nye = Local.with_ymd_and_hms(2026, 12, 31, 23, 0, 0).unwrap();
        let u = parse("Current session: 9% used · resets Jan 1 at 4:00am (X)", nye).unwrap();
        assert_eq!(
            u.session_resets_at.unwrap().format("%Y-%m-%d").to_string(),
            "2027-01-01"
        );
    }

    #[test]
    fn unrecognized_output_yields_none_rather_than_wrong_numbers() {
        // The format is prose and can change. Reporting nothing is recoverable;
        // reporting a confidently wrong window is not.
        assert!(parse("", Local::now()).is_none());
        assert!(parse("Something entirely different", Local::now()).is_none());
        assert!(parse("Current session: unavailable", Local::now()).is_none());
    }

    #[test]
    fn a_missing_reset_time_still_keeps_the_percentage() {
        let u = parse("Current session: 42% used", aug8()).unwrap();
        assert_eq!(u.session_pct, Some(42));
        assert_eq!(u.session_resets_at, None);
    }
}
