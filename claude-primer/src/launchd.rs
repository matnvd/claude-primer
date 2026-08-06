use crate::config::{self, Config, AGENT_LABEL, DAEMON_LABEL};
use anyhow::{anyhow, Context, Result};
use plist::Value;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;

pub fn agent_plist_path() -> Result<PathBuf> {
    Ok(config::home()?.join("Library/LaunchAgents").join(format!("{AGENT_LABEL}.plist")))
}

pub fn daemon_plist_path() -> PathBuf {
    PathBuf::from("/Library/LaunchDaemons").join(format!("{DAEMON_LABEL}.plist"))
}

/// launchd's Weekday is 0-6 with Sunday as 0.
fn launchd_weekday(w: chrono::Weekday) -> u32 {
    w.num_days_from_sunday()
}

/// The LaunchAgent runs the primes. It carries `CLAUDE_CODE_OAUTH_TOKEN` in its
/// environment so a prime works even when the login keychain isn't unlocked — that
/// token outranks the keychain credential in Claude Code's auth precedence.
///
/// Written at mode 0600 because it contains that token.
pub fn write_agent(cfg: &Config, exe: &str, token: Option<&str>) -> Result<PathBuf> {
    let path = agent_plist_path()?;
    std::fs::create_dir_all(path.parent().unwrap())?;

    let mut env = BTreeMap::new();
    if let Some(t) = token {
        env.insert("CLAUDE_CODE_OAUTH_TOKEN".to_string(), Value::String(t.to_string()));
    }
    // launchd hands jobs a minimal PATH; give `claude` a sane one for anything it shells out to.
    env.insert("PATH".to_string(), Value::String("/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin".into()));

    // One StartCalendarInterval entry per (weekday, anchor), taking each day's own
    // anchor set so a weekend schedule emits its own times. A launchd job carries a
    // single argv across all of its calendar entries, so the anchor cannot be baked
    // into the arguments — `run --anchor auto` resolves which one fired from the clock.
    let mut jobs: Vec<Value> = Vec::new();
    let mode = cfg.mode()?;
    for (weekday, anchors) in cfg.active_days()? {
        let probe = config::nearest_date_with_weekday(cfg.today()?, weekday);
        for anchor in anchors {
            let (h, m, local_weekday) = anchor.local_hm(probe, mode)?;
            let mut d = BTreeMap::new();
            d.insert("Hour".to_string(), Value::Integer((h as i64).into()));
            d.insert("Minute".to_string(), Value::Integer((m as i64).into()));
            d.insert("Weekday".to_string(), Value::Integer((launchd_weekday(local_weekday) as i64).into()));
            jobs.push(Value::Dictionary(d.into_iter().collect()));
        }
    }

    let mut root: BTreeMap<String, Value> = BTreeMap::new();
    root.insert("Label".into(), Value::String(AGENT_LABEL.into()));
    root.insert(
        "ProgramArguments".into(),
        Value::Array(vec![
            Value::String(exe.into()),
            Value::String("run".into()),
            Value::String("--anchor".into()),
            Value::String("auto".into()),
        ]),
    );
    root.insert("StartCalendarInterval".into(), Value::Array(jobs));
    root.insert("EnvironmentVariables".into(), Value::Dictionary(env.into_iter().collect()));
    root.insert("RunAtLoad".into(), Value::Boolean(false));
    root.insert("ProcessType".into(), Value::String("Background".into()));
    root.insert(
        "StandardOutPath".into(),
        Value::String(config::data_dir()?.join("agent.out.log").display().to_string()),
    );
    root.insert(
        "StandardErrorPath".into(),
        Value::String(config::data_dir()?.join("agent.err.log").display().to_string()),
    );

    let value = Value::Dictionary(root.into_iter().collect());
    value.to_file_xml(&path).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(path)
}

/// The LaunchDaemon runs as root purely so it can call `pmset`, which a LaunchAgent
/// cannot do unattended. It re-arms the rolling one-time wakes once a day.
pub fn daemon_plist_xml(exe: &str) -> Result<Vec<u8>> {
    let mut interval = BTreeMap::new();
    interval.insert("Hour".to_string(), Value::Integer(3i64.into()));
    interval.insert("Minute".to_string(), Value::Integer(15i64.into()));

    let mut root: BTreeMap<String, Value> = BTreeMap::new();
    root.insert("Label".into(), Value::String(DAEMON_LABEL.into()));
    root.insert(
        "ProgramArguments".into(),
        Value::Array(vec![Value::String(exe.into()), Value::String("arm-wakes".into())]),
    );
    root.insert(
        "StartCalendarInterval".into(),
        Value::Array(vec![Value::Dictionary(interval.into_iter().collect())]),
    );
    root.insert("RunAtLoad".into(), Value::Boolean(true));
    root.insert("StandardOutPath".into(), Value::String("/tmp/claude-primer-wake.out.log".into()));
    root.insert("StandardErrorPath".into(), Value::String("/tmp/claude-primer-wake.err.log".into()));

    let mut buf = Vec::new();
    plist::to_writer_xml(&mut buf, &Value::Dictionary(root.into_iter().collect()))?;
    Ok(buf)
}

pub fn uid() -> u32 {
    // SAFETY: getuid is always safe; it takes no arguments and cannot fail.
    unsafe { libc_getuid() }
}

extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

pub fn bootstrap_agent(path: &PathBuf) -> Result<()> {
    // bootout first so a re-install replaces cleanly; failure is fine when not loaded.
    let _ = Command::new("/bin/launchctl")
        .args(["bootout", &format!("gui/{}/{}", uid(), AGENT_LABEL)])
        .output();
    let out = Command::new("/bin/launchctl")
        .args(["bootstrap", &format!("gui/{}", uid()), &path.display().to_string()])
        .output()
        .context("launchctl bootstrap failed to execute")?;
    if !out.status.success() {
        return Err(anyhow!(
            "launchctl bootstrap: {}",
            String::from_utf8_lossy(&out.stderr).trim().to_string()
        ));
    }
    Ok(())
}

pub fn bootout_agent() -> Result<bool> {
    let out = Command::new("/bin/launchctl")
        .args(["bootout", &format!("gui/{}/{}", uid(), AGENT_LABEL)])
        .output()?;
    Ok(out.status.success())
}

/// `launchctl list` columns are PID, LastExitStatus, Label. A `-` PID means loaded
/// but idle, which is the normal resting state between primes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitStatus {
    pub loaded: bool,
    pub pid: Option<u32>,
    pub last_exit: Option<i32>,
}

pub fn parse_launchctl_list(output: &str, label: &str) -> UnitStatus {
    for line in output.lines() {
        let mut cols = line.split_whitespace();
        let (pid, status, found) = (cols.next(), cols.next(), cols.next());
        if found == Some(label) {
            return UnitStatus {
                loaded: true,
                pid: pid.and_then(|p| p.parse().ok()),
                last_exit: status.and_then(|s| s.parse().ok()),
            };
        }
    }
    UnitStatus { loaded: false, pid: None, last_exit: None }
}

pub fn unit_status(label: &str) -> UnitStatus {
    let out = Command::new("/bin/launchctl").arg("list").output();
    match out {
        Ok(o) => parse_launchctl_list(&String::from_utf8_lossy(&o.stdout), label),
        Err(_) => UnitStatus { loaded: false, pid: None, last_exit: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Weekday};

    #[test]
    fn launchd_weekdays_are_sunday_zero() {
        assert_eq!(launchd_weekday(Weekday::Sun), 0);
        assert_eq!(launchd_weekday(Weekday::Mon), 1);
        assert_eq!(launchd_weekday(Weekday::Sat), 6);
    }

    #[test]
    fn launchctl_list_reports_loaded_and_idle() {
        let out = "PID\tStatus\tLabel\n-\t0\tcom.claude-primer.agent\n123\t0\tcom.other\n";
        let s = parse_launchctl_list(out, "com.claude-primer.agent");
        assert!(s.loaded);
        assert_eq!(s.pid, None); // '-' means loaded but not executing
        assert_eq!(s.last_exit, Some(0));
    }

    #[test]
    fn launchctl_list_reports_a_running_pid() {
        let out = "9876\t0\tcom.claude-primer.agent\n";
        let s = parse_launchctl_list(out, "com.claude-primer.agent");
        assert_eq!(s.pid, Some(9876));
    }

    #[test]
    fn launchctl_list_surfaces_a_failing_exit_code() {
        let out = "-\t1\tcom.claude-primer.agent\n";
        let s = parse_launchctl_list(out, "com.claude-primer.agent");
        assert!(s.loaded);
        assert_eq!(s.last_exit, Some(1));
    }

    #[test]
    fn an_absent_label_is_not_loaded() {
        let s = parse_launchctl_list("123\t0\tcom.other\n", "com.claude-primer.agent");
        assert!(!s.loaded);
    }

    #[test]
    fn nearest_date_lands_on_the_requested_weekday() {
        let start = chrono::NaiveDate::from_ymd_opt(2026, 8, 6).unwrap(); // a Thursday
        assert_eq!(config::nearest_date_with_weekday(start, Weekday::Thu), start);
        assert_eq!(config::nearest_date_with_weekday(start, Weekday::Sat).weekday(), Weekday::Sat);
    }

    #[test]
    fn daemon_plist_is_valid_xml_naming_arm_wakes() {
        let xml = String::from_utf8(daemon_plist_xml("/usr/local/bin/claude-primer").unwrap()).unwrap();
        assert!(xml.contains("com.claude-primer.wake"));
        assert!(xml.contains("arm-wakes"));
        assert!(xml.contains("<plist"));
    }
}
