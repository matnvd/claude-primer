use crate::config::{self, Config, PMSET_OWNER};
use crate::state::{ArmedWake, WakeLedger};
use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Local};
use std::process::Command;

const PMSET: &str = "/usr/bin/pmset";

/// Wake this far ahead of the anchor so the machine is fully up before launchd fires.
const WAKE_LEAD: i64 = 2;

/// How many days of one-time wakes to keep armed. The daemon re-arms daily, so this
/// is a safety horizon rather than a schedule.
const HORIZON_DAYS: i64 = 7;

pub fn fmt_pmset_datetime(dt: &DateTime<Local>) -> String {
    dt.format("%m/%d/%y %H:%M:%S").to_string()
}

/// Arm wake events for every upcoming anchor.
///
/// `man pmset`: *"you may only have one pair of repeating events scheduled"*. That
/// single slot goes to the earliest anchor, which is the one that genuinely needs it
/// (the Mac is asleep overnight). Later anchors use one-time `schedule` events, which
/// may exist in quantity but must be re-armed — hence the daily daemon.
pub fn arm(cfg: &Config) -> Result<WakeLedger> {
    let mut ledger = WakeLedger::load()?;

    // Cancel only what we armed before, by exact match. `cancelall` would also take
    // out the system's calendar-alarm and analytics wakes.
    for w in std::mem::take(&mut ledger.armed) {
        let _ = cancel_one(&w.datetime);
    }

    let anchors = cfg.anchors()?;
    let earliest = anchors.first().copied().ok_or_else(|| anyhow!("no anchors configured"))?;

    // The one repeating slot: earliest anchor, on the scheduled weekdays.
    let probe = config::today_edt();
    let (h, m, _) = earliest.local_hm(probe)?;
    let time_local = format!("{h:02}:{m:02}:00");
    let weekdays: String = cfg
        .weekday_set()?
        .into_iter()
        .map(config::pmset_weekday_char)
        .collect();
    set_repeat(&time_local, &weekdays)?;
    ledger.repeat_time_local = Some(time_local);
    ledger.repeat_weekdays = Some(weekdays);

    // One-time wakes for the remaining anchors across the horizon.
    let now = Local::now();
    for offset in 0..HORIZON_DAYS {
        let date = config::today_edt() + Duration::days(offset);
        if !cfg.runs_on(date)? {
            continue;
        }
        for anchor in anchors.iter().skip(1) {
            let due = anchor.local_on(date)?;
            let wake_at = due - Duration::minutes(WAKE_LEAD);
            if wake_at <= now {
                continue;
            }
            let stamp = fmt_pmset_datetime(&wake_at);
            schedule_wake(&stamp)?;
            ledger.armed.push(ArmedWake { datetime: stamp, for_anchor: anchor.label() });
        }
    }

    ledger.save()?;
    Ok(ledger)
}

/// Cancel everything we armed and clear the repeating slot. Only our own events.
pub fn disarm() -> Result<usize> {
    let mut ledger = WakeLedger::load()?;
    let mut cancelled = 0;
    for w in std::mem::take(&mut ledger.armed) {
        if cancel_one(&w.datetime).is_ok() {
            cancelled += 1;
        }
    }
    if ledger.repeat_time_local.is_some() {
        let _ = Command::new(PMSET).args(["repeat", "cancel"]).output();
        ledger.repeat_time_local = None;
        ledger.repeat_weekdays = None;
    }
    ledger.save()?;
    Ok(cancelled)
}

fn schedule_wake(stamp: &str) -> Result<()> {
    run_pmset(&["schedule", "wake", stamp, PMSET_OWNER])
}

fn cancel_one(stamp: &str) -> Result<()> {
    run_pmset(&["schedule", "cancel", "wake", stamp, PMSET_OWNER])
}

fn set_repeat(time_local: &str, weekdays: &str) -> Result<()> {
    run_pmset(&["repeat", "wakeorpoweron", weekdays, time_local])
}

fn run_pmset(args: &[&str]) -> Result<()> {
    let out = Command::new(PMSET)
        .args(args)
        .output()
        .with_context(|| format!("could not execute {PMSET}"))?;
    if !out.status.success() {
        return Err(anyhow!(
            "pmset {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim().to_string()
        ));
    }
    Ok(())
}

/// Read back the live schedule so `status` can show what is actually armed rather
/// than what we believe we armed.
pub fn scheduled_events() -> Vec<String> {
    let out = Command::new(PMSET).args(["-g", "sched"]).output();
    match out {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty() && !l.starts_with("Scheduled power events"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub fn ours(events: &[String]) -> Vec<&String> {
    events.iter().filter(|e| e.contains(PMSET_OWNER)).collect()
}

pub fn is_root() -> bool {
    // SAFETY: geteuid takes no arguments and cannot fail.
    unsafe { geteuid() == 0 }
}

extern "C" {
    fn geteuid() -> u32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn pmset_datetime_uses_the_documented_format() {
        // man pmset: date/time is "MM/dd/yy HH:mm:ss" in 24-hour format.
        let dt = Local.with_ymd_and_hms(2026, 8, 7, 10, 28, 0).unwrap();
        assert_eq!(fmt_pmset_datetime(&dt), "08/07/26 10:28:00");
    }

    #[test]
    fn midnight_and_single_digit_dates_stay_zero_padded() {
        let dt = Local.with_ymd_and_hms(2026, 1, 3, 0, 5, 0).unwrap();
        assert_eq!(fmt_pmset_datetime(&dt), "01/03/26 00:05:00");
    }

    #[test]
    fn weekday_string_matches_pmset_alphabet() {
        let cfg = Config::default();
        let s: String = cfg.weekday_set().unwrap().into_iter().map(config::pmset_weekday_char).collect();
        assert_eq!(s, "MTWRF");
    }

    #[test]
    fn our_events_are_identified_by_owner() {
        let events = vec![
            " [0]  wake at 08/07/26 10:28:00 by 'claude-primer'".to_string(),
            " [1]  wake at 08/07/26 00:00:00 by 'com.apple.alarm'".to_string(),
        ];
        let mine = ours(&events);
        assert_eq!(mine.len(), 1);
        assert!(mine[0].contains("claude-primer"));
    }

    #[test]
    fn cancelall_is_never_constructed() {
        // Guards against a regression that would destroy the system's own wakes.
        let src = include_str!("pmset.rs");
        assert!(!src.contains("\"cancelall\""));
    }
}
