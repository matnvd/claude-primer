use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, NaiveTime, TimeZone, Weekday};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    /// The base anchor set, used on every day listed in `weekdays`.
    pub anchors: Vec<String>,
    pub weekdays: Vec<String>,
    /// Per-day overrides, e.g. `[schedules] Sat = ["09:00", "14:00"]`. A day listed
    /// here uses its own anchors and runs whether or not it appears in `weekdays`,
    /// so a weekend schedule needs only this table. An empty list disables a day.
    #[serde(default)]
    pub schedules: BTreeMap<String, Vec<String>>,
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
            // No weekend entries by default; add e.g. Sat = ["09:00", "14:00"] to opt in.
            schedules: BTreeMap::new(),
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

    /// Serializing the struct discards every comment in the file, so this is only
    /// used to create a config that does not exist yet. Never call it to "update" a
    /// config the user has edited — see `write_default_if_absent`.
    pub fn save(&self) -> Result<()> {
        let p = Self::path()?;
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Write a commented starter config, but only when none exists. Returns whether
    /// a file was created, so `install` can leave a hand-edited config untouched.
    pub fn write_default_if_absent(claude_bin: &str) -> Result<bool> {
        let p = Self::path()?;
        if p.exists() {
            return Ok(false);
        }
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(&p, default_config_toml(claude_bin))?;
        Ok(true)
    }

    /// The base anchor set. Prefer `anchors_for` — this ignores per-day overrides.
    pub fn anchors(&self) -> Result<Vec<Anchor>> {
        self.anchors.iter().map(|s| Anchor::parse(s)).collect()
    }

    pub fn weekday_set(&self) -> Result<Vec<Weekday>> {
        self.weekdays.iter().map(|d| parse_weekday(d)).collect()
    }

    /// The anchors that apply on a given EDT-local date, sorted.
    ///
    /// A `[schedules]` entry for that weekday wins outright, and makes the day active
    /// even when it is absent from `weekdays` — which is how a weekend gets its own
    /// times. Otherwise the base `anchors` apply if the day is in `weekdays`. Neither
    /// case matching means the day is off.
    ///
    /// The weekday is taken in EDT because that is the zone anchors are declared in.
    pub fn anchors_for(&self, date_edt: NaiveDate) -> Result<Vec<Anchor>> {
        let want = date_edt.weekday();
        for (day, times) in &self.schedules {
            if parse_weekday(day)? == want {
                let mut a: Vec<Anchor> = times.iter().map(|s| Anchor::parse(s)).collect::<Result<_>>()?;
                a.sort();
                return Ok(a);
            }
        }
        if self.weekday_set()?.contains(&want) {
            let mut a = self.anchors()?;
            a.sort();
            return Ok(a);
        }
        Ok(Vec::new())
    }

    /// Whether anything is scheduled on this date.
    pub fn runs_on(&self, date_edt: NaiveDate) -> Result<bool> {
        Ok(!self.anchors_for(date_edt)?.is_empty())
    }

    /// Every weekday that has at least one anchor, with the anchors that apply.
    pub fn active_days(&self) -> Result<Vec<(Weekday, Vec<Anchor>)>> {
        const ALL: [Weekday; 7] = [
            Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu,
            Weekday::Fri, Weekday::Sat, Weekday::Sun,
        ];
        let today = today_edt();
        let mut out = Vec::new();
        for w in ALL {
            let probe = nearest_date_with_weekday(today, w);
            let anchors = self.anchors_for(probe)?;
            if !anchors.is_empty() {
                out.push((w, anchors));
            }
        }
        Ok(out)
    }
}

/// The next date on or after `from` that falls on `want`.
pub fn nearest_date_with_weekday(from: NaiveDate, want: Weekday) -> NaiveDate {
    let mut d = from;
    for _ in 0..7 {
        if d.weekday() == want {
            return d;
        }
        d = d.succ_opt().unwrap_or(d);
    }
    from
}

/// A commented starter config. Comments survive because nothing rewrites the file
/// once it exists — `install` reads it and leaves it alone.
pub fn default_config_toml(claude_bin: &str) -> String {
    format!(
        r##"# claude-primer configuration
# Comments use '#'. This file is never rewritten once it exists, so anything you
# add here is safe. Re-run `claude-primer install` after editing: anchor times are
# baked into the launchd job, so edits do not take effect until you reinstall.

# Absolute path — launchd gives jobs a minimal PATH that excludes ~/.local/bin.
claude_bin = "{claude_bin}"

# Times to send a priming prompt, declared in EDT (fixed UTC-4, no DST shifting).
# `status` shows each one converted to your Mac's clock.
anchors  = ["05:30", "10:30", "15:30"]

# Days the `anchors` above apply to. Days listed in neither this nor [schedules]
# are off, and a run on one exits without spending anything.
weekdays = ["Mon", "Tue", "Wed", "Thu", "Fri"]

# haiku is the cheapest and stays off the Sonnet-specific weekly cap.
model = "haiku"

timezone = "EDT"

# When to post a macOS notification: "failure" | "never" | "always".
# On "failure", silence means everything worked.
notify_on = "failure"

# What to do when an anchor passed while the Mac was off and launchd catches up late:
#   "skip"  - do nothing (default). Firing late would open a misaligned window and
#             shift every later boundary, which is worse than missing one.
#   "shift" - prime anyway and accept the shifted window.
on_missed = "skip"

# How late an anchor may fire and still count as on time.
grace_minutes = 20

# Per-day schedules. A day here uses its own times and is active whether or not it
# appears in `weekdays`. An empty list turns a day off. Uncomment to use:
#
# [schedules]
# Sat = ["09:00", "14:00"]
# Sun = ["11:00"]
#
# For a fully per-day setup, set anchors = [] and weekdays = [] above and list all
# seven days here. [schedules] must stay at the end of the file.
"##
    )
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

    fn on(cfg: &Config, weekday: Weekday) -> Vec<String> {
        let d = nearest_date_with_weekday(NaiveDate::from_ymd_opt(2026, 8, 3).unwrap(), weekday);
        cfg.anchors_for(d).unwrap().iter().map(|a| a.label()).collect()
    }

    #[test]
    fn the_generated_config_is_commented_and_parses() {
        let src = default_config_toml("/Users/x/.local/bin/claude");
        assert!(src.contains('#'), "starter config should explain itself");
        let c: Config = toml::from_str(&src).expect("generated config must parse");
        assert_eq!(c.claude_bin, "/Users/x/.local/bin/claude");
        assert_eq!(c.anchors, vec!["05:30", "10:30", "15:30"]);
        // [schedules] is shown commented out, so it must parse as absent.
        assert!(c.schedules.is_empty());
    }

    #[test]
    fn comments_survive_a_parse_but_not_a_serialize() {
        // Documents exactly why install must not rewrite an existing config:
        // round-tripping through the struct silently discards every comment.
        let src = default_config_toml("/bin/claude");
        let round_tripped = toml::to_string_pretty(&toml::from_str::<Config>(&src).unwrap()).unwrap();
        assert!(!round_tripped.contains('#'), "serializing drops comments — hence write_default_if_absent");
    }

    #[test]
    fn weekends_are_off_by_default() {
        let c = Config::default();
        assert!(on(&c, Weekday::Sat).is_empty());
        assert!(on(&c, Weekday::Sun).is_empty());
        assert_eq!(on(&c, Weekday::Mon), vec!["05:30", "10:30", "15:30"]);
    }

    #[test]
    fn a_per_day_schedule_overrides_the_base_anchors() {
        let mut c = Config::default();
        c.schedules.insert("Sat".into(), vec!["09:00".into(), "14:00".into()]);
        assert_eq!(on(&c, Weekday::Sat), vec!["09:00", "14:00"]);
        // Weekdays keep the base set.
        assert_eq!(on(&c, Weekday::Mon), vec!["05:30", "10:30", "15:30"]);
    }

    #[test]
    fn a_per_day_schedule_activates_a_day_absent_from_weekdays() {
        // Saturday is not in `weekdays`, but a [schedules] entry is enough on its own.
        let mut c = Config::default();
        c.schedules.insert("Sun".into(), vec!["10:00".into()]);
        let d = nearest_date_with_weekday(NaiveDate::from_ymd_opt(2026, 8, 3).unwrap(), Weekday::Sun);
        assert!(c.runs_on(d).unwrap());
    }

    #[test]
    fn an_empty_per_day_list_disables_that_day() {
        let mut c = Config::default();
        c.schedules.insert("Wed".into(), vec![]);
        let d = nearest_date_with_weekday(NaiveDate::from_ymd_opt(2026, 8, 3).unwrap(), Weekday::Wed);
        assert!(!c.runs_on(d).unwrap());
        assert!(on(&c, Weekday::Wed).is_empty());
    }

    #[test]
    fn per_day_anchors_are_sorted() {
        let mut c = Config::default();
        c.schedules.insert("Sat".into(), vec!["14:00".into(), "09:00".into()]);
        assert_eq!(on(&c, Weekday::Sat), vec!["09:00", "14:00"]);
    }

    #[test]
    fn active_days_lists_weekdays_and_overrides_together() {
        let mut c = Config::default();
        c.schedules.insert("Sat".into(), vec!["09:00".into()]);
        let days: Vec<Weekday> = c.active_days().unwrap().into_iter().map(|(w, _)| w).collect();
        assert_eq!(days, vec![
            Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri, Weekday::Sat
        ]);
    }

    #[test]
    fn schedules_round_trip_through_toml() {
        let mut c = Config::default();
        c.claude_bin = "/bin/claude".into();
        c.schedules.insert("Sat".into(), vec!["09:00".into(), "14:00".into()]);
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.schedules.get("Sat").unwrap(), &vec!["09:00", "14:00"]);
    }

    #[test]
    fn a_config_without_schedules_still_parses() {
        // Backward compatibility: `schedules` is #[serde(default)].
        let toml_src = r#"
claude_bin = "/bin/claude"
anchors = ["05:30"]
weekdays = ["Mon"]
model = "haiku"
timezone = "EDT"
notify_on = "failure"
on_missed = "skip"
grace_minutes = 20
"#;
        let c: Config = toml::from_str(toml_src).unwrap();
        assert!(c.schedules.is_empty());
        assert_eq!(on(&c, Weekday::Mon), vec!["05:30"]);
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
