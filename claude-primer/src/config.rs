use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, NaiveTime, TimeZone, Weekday};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Anchors are declared in fixed EDT (UTC-4) with no automatic DST adjustment.
/// `StartCalendarInterval` has no timezone field — launchd fires in the *system*
/// zone — so every anchor is converted to system-local before it reaches a plist
/// or a `pmset` datetime. See `Anchor::local_on`.
pub const EDT_OFFSET_SECS: i32 = 4 * 3600;

pub fn edt() -> FixedOffset {
    FixedOffset::west_opt(EDT_OFFSET_SECS).expect("EDT offset is in range")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Absolute path. launchd hands jobs a minimal PATH (/usr/bin:/bin:/usr/sbin:/sbin),
    /// which does not include ~/.local/bin — resolving this at install time is what
    /// keeps the scheduled job from silently doing nothing.
    pub claude_bin: String,
    pub anchors: Vec<String>,
    pub weekdays: Vec<String>,
    pub model: String,
    pub timezone: String,
    pub notify_on: NotifyOn,
    pub on_missed: OnMissed,
    pub grace_minutes: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyOn {
    Failure,
    Never,
    Always,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnMissed {
    /// Past the grace period, skip and spend nothing. launchd's StartCalendarInterval
    /// catches up after the Mac was off; firing then would open a misaligned window
    /// and shift every later boundary.
    Skip,
    /// Prime anyway and accept the shifted window.
    Shift,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            claude_bin: String::new(),
            // anchors!!!
            anchors: vec!["05:30".into(), "10:30".into(), "15:30".into()],
            weekdays: vec!["Mon".into(), "Tue".into(), "Wed".into(), "Thu".into(), "Fri".into()],
            model: "haiku".into(),
            timezone: "EDT".into(),
            notify_on: NotifyOn::Failure,
            on_missed: OnMissed::Skip,
            grace_minutes: 20,
        }
    }
}

impl Config {
    pub fn path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.toml"))
    }

    pub fn load() -> Result<Self> {
        let p = Self::path()?;
        let raw = std::fs::read_to_string(&p)
            .with_context(|| format!("no config at {} — run `claude-primer install`", p.display()))?;
        toml::from_str(&raw).with_context(|| format!("could not parse {}", p.display()))
    }

    pub fn save(&self) -> Result<()> {
        let p = Self::path()?;
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    pub fn anchors(&self) -> Result<Vec<Anchor>> {
        self.anchors.iter().map(|s| Anchor::parse(s)).collect()
    }

    pub fn weekday_set(&self) -> Result<Vec<Weekday>> {
        self.weekdays.iter().map(|d| parse_weekday(d)).collect()
    }

    /// Whether the given EDT-local date is a scheduled day. The weekday is taken in
    /// EDT because that is the zone the anchors are declared in.
    pub fn runs_on(&self, date_edt: NaiveDate) -> Result<bool> {
        Ok(self.weekday_set()?.contains(&date_edt.weekday()))
    }
}

/// A wall-clock time of day, declared in EDT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Anchor {
    pub hour: u32,
    pub minute: u32,
}

impl Anchor {
    pub fn parse(s: &str) -> Result<Self> {
        let t = NaiveTime::parse_from_str(s.trim(), "%H:%M")
            .with_context(|| format!("anchor {s:?} is not HH:MM"))?;
        Ok(Self { hour: t.format("%H").to_string().parse()?, minute: t.format("%M").to_string().parse()? })
    }

    pub fn label(&self) -> String {
        format!("{:02}:{:02}", self.hour, self.minute)
    }

    /// This anchor on `date_edt`, as an instant, then expressed in system-local time.
    pub fn local_on(&self, date_edt: NaiveDate) -> Result<DateTime<Local>> {
        let naive = date_edt
            .and_hms_opt(self.hour, self.minute, 0)
            .ok_or_else(|| anyhow!("invalid anchor time {}", self.label()))?;
        let in_edt = edt()
            .from_local_datetime(&naive)
            .single()
            .ok_or_else(|| anyhow!("ambiguous EDT time {}", self.label()))?;
        Ok(in_edt.with_timezone(&Local))
    }

    /// The same wall-clock time as the system would express it, e.g. 05:30 EDT on a
    /// UTC-3 machine is 06:30 local. This is what goes into StartCalendarInterval.
    pub fn local_hm(&self, date_edt: NaiveDate) -> Result<(u32, u32, Weekday)> {
        let dt = self.local_on(date_edt)?;
        Ok((
            dt.format("%H").to_string().parse()?,
            dt.format("%M").to_string().parse()?,
            dt.date_naive().weekday(),
        ))
    }
}

pub fn parse_weekday(s: &str) -> Result<Weekday> {
    match s.trim().to_ascii_lowercase().as_str() {
        "mon" | "monday" => Ok(Weekday::Mon),
        "tue" | "tues" | "tuesday" => Ok(Weekday::Tue),
        "wed" | "weds" | "wednesday" => Ok(Weekday::Wed),
        "thu" | "thur" | "thurs" | "thursday" => Ok(Weekday::Thu),
        "fri" | "friday" => Ok(Weekday::Fri),
        "sat" | "saturday" => Ok(Weekday::Sat),
        "sun" | "sunday" => Ok(Weekday::Sun),
        other => Err(anyhow!("unrecognized weekday {other:?}")),
    }
}

/// `pmset repeat` takes weekdays as a subset of MTWRFSU.
pub fn pmset_weekday_char(w: Weekday) -> char {
    match w {
        Weekday::Mon => 'M',
        Weekday::Tue => 'T',
        Weekday::Wed => 'W',
        Weekday::Thu => 'R',
        Weekday::Fri => 'F',
        Weekday::Sat => 'S',
        Weekday::Sun => 'U',
    }
}

pub fn today_edt() -> NaiveDate {
    Local::now().with_timezone(&edt()).date_naive()
}

pub fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set"))
}

pub fn config_dir() -> Result<PathBuf> {
    Ok(home()?.join(".config/claude-primer"))
}

pub fn data_dir() -> Result<PathBuf> {
    Ok(home()?.join(".local/share/claude-primer"))
}

/// Primes run here so no project CLAUDE.md, hooks, or .mcp.json are discovered, and
/// so their transcripts are filed under their own project folder rather than polluting
/// the --resume history of real work.
pub fn prime_cwd() -> Result<PathBuf> {
    Ok(data_dir()?.join("empty"))
}

pub fn runs_log() -> Result<PathBuf> {
    Ok(data_dir()?.join("runs.jsonl"))
}

pub fn wake_ledger() -> Result<PathBuf> {
    Ok(data_dir()?.join("wakes.json"))
}

pub const AGENT_LABEL: &str = "com.claude-primer.agent";
pub const DAEMON_LABEL: &str = "com.claude-primer.wake";
/// Identifies our pmset events so cancellation can target them exactly.
/// `pmset schedule cancelall` must never be used — it would take out the system's
/// own calendar-alarm and analytics wakes too.
pub const PMSET_OWNER: &str = "claude-primer";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_parses_and_labels() {
        let a = Anchor::parse("05:30").unwrap();
        assert_eq!((a.hour, a.minute), (5, 30));
        assert_eq!(a.label(), "05:30");
        assert!(Anchor::parse("25:00").is_err());
        assert!(Anchor::parse("bogus").is_err());
    }

    #[test]
    fn edt_is_utc_minus_four() {
        assert_eq!(edt().local_minus_utc(), -4 * 3600);
    }

    #[test]
    fn anchor_converts_to_a_fixed_instant() {
        // 05:30 EDT is 09:30 UTC regardless of what the system zone is.
        let date = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let dt = Anchor::parse("05:30").unwrap().local_on(date).unwrap();
        assert_eq!(dt.naive_utc().format("%H:%M").to_string(), "09:30");
    }

    #[test]
    fn weekday_parsing_and_pmset_chars() {
        assert_eq!(parse_weekday("Mon").unwrap(), Weekday::Mon);
        assert_eq!(parse_weekday("thursday").unwrap(), Weekday::Thu);
        assert!(parse_weekday("Funday").is_err());
        assert_eq!(pmset_weekday_char(Weekday::Thu), 'R');
        assert_eq!(pmset_weekday_char(Weekday::Sun), 'U');
    }
}
