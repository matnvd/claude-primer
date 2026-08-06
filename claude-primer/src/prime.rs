use crate::config::{self, Anchor, Config, NotifyOn, OnMissed};
use crate::state::{self, Outcome, RunRecord};
use anyhow::{Context, Result};
use chrono::Local;
use std::process::Command;

/// The prompt and system prompt are deliberately trivial. `--system-prompt` replaces
/// Claude Code's default system prompt, which is the dominant token cost of a `-p` run.
const PROMPT: &str = "ok";
const SYSTEM_PROMPT: &str = "Reply with exactly: OK";

pub struct PrimeArgs {
    pub anchor: String,
    pub dry_run: bool,
    pub force: bool,
}

/// Build the exact argument vector for a prime.
///
/// Every flag here is load-bearing, and the measured cost of dropping any of them is
/// large. A prime with only `--system-prompt` and `--model haiku` still cost $0.0226
/// and sent 11,139 input tokens; with the full set below it costs $0.000675 and sends
/// 240. That is 33x cheaper, and 46x fewer tokens against the weekly cap.
///
/// - `--system-prompt` replaces the default system prompt (measured: prompt drops to
///   ~10 tokens)
/// - `--tools ""` removes every built-in tool *definition* from the model's context.
///   This was the dominant cost: the tool schemas alone were 11,129 tokens. A prime
///   needs no tools, so they are pure waste.
/// - `--strict-mcp-config` + empty config stops global MCP servers from loading
/// - `--settings` overrides a global `effortLevel` that would otherwise apply
/// - `--model haiku` keeps the call off the Sonnet-specific weekly cap
/// - `--max-turns 1` bounds it to a single turn
///
/// `--bare` is deliberately absent: bare mode never reads OAuth credentials or
/// `CLAUDE_CODE_OAUTH_TOKEN`, so it would bill the API instead of touching the
/// subscription window — the opposite of the point.
pub fn build_args(cfg: &Config) -> Vec<String> {
    vec![
        "-p".into(),
        PROMPT.into(),
        "--model".into(),
        cfg.model.clone(),
        "--system-prompt".into(),
        SYSTEM_PROMPT.into(),
        "--tools".into(),
        String::new(),
        "--max-turns".into(),
        "1".into(),
        "--output-format".into(),
        "json".into(),
        "--settings".into(),
        r#"{"effortLevel":"low"}"#.into(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        r#"{"mcpServers":{}}"#.into(),
    ]
}

/// Prompt caching is counterproductive for a prime. Primes are 5 hours apart, so a
/// cache entry (1-hour TTL) is always cold, and every run paid the cache-*write*
/// premium for nothing. Disabling it removed the entire cache_creation charge.
pub fn build_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("CLAUDE_CODE_DISABLE_PROMPT_CACHING", "1"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
    ]
}

pub fn render_command(cfg: &Config) -> String {
    let mut parts: Vec<String> = build_env().iter().map(|(k, v)| format!("{k}={v}")).collect();
    parts.push(shell_quote(&cfg.claude_bin));
    parts.extend(build_args(cfg).iter().map(|a| shell_quote(a)));
    parts.join(" ")
}

fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "-_./:".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Which anchor just fired.
///
/// A launchd job carries one argv across all of its StartCalendarInterval entries,
/// so the agent invokes `run --anchor auto` and the anchor is recovered from the
/// clock: the latest anchor already due, allowing a small tolerance for launchd
/// firing a hair early. After a catch-up this resolves to the anchor that was
/// missed, which is exactly what the staleness guard needs to see.
pub fn resolve_auto(cfg: &Config, now: chrono::DateTime<Local>) -> Result<Anchor> {
    const EARLY_TOLERANCE_MINS: i64 = 2;
    let date = config::today_edt();
    // Today's own anchor set, so a weekend override resolves against its own times.
    let mut anchors = cfg.anchors_for(date)?;
    if anchors.is_empty() {
        anchors = cfg.anchors()?;
    }
    anchors.sort();

    let mut best: Option<Anchor> = None;
    for a in &anchors {
        let due = a.local_on(date)?;
        if (now - due).num_minutes() >= -EARLY_TOLERANCE_MINS {
            best = Some(*a);
        }
    }
    // Fired before the first anchor of the day: attribute it to that first anchor.
    best.or_else(|| anchors.first().copied())
        .ok_or_else(|| anyhow::anyhow!("no anchors configured"))
}

pub fn run(cfg: &Config, args: PrimeArgs) -> Result<Outcome> {
    let now = Local::now();

    let args = if args.anchor == "auto" {
        let resolved = resolve_auto(cfg, now)?;
        PrimeArgs { anchor: resolved.label(), ..args }
    } else {
        args
    };

    let scheduled = Anchor::parse(&args.anchor).ok();

    // A non-HH:MM label (e.g. `--anchor test`) is a manual invocation: the schedule
    // guards below don't apply because there is no scheduled time to be late for.
    if let (Some(anchor), false) = (scheduled, args.force) {
        let date = config::today_edt();

        if !cfg.runs_on(date)? {
            let rec = RunRecord::new(&args.anchor, Outcome::SkippedNotScheduled);
            state::append_run(&rec)?;
            println!("{} — {}", anchor.label(), Outcome::SkippedNotScheduled.label());
            return Ok(Outcome::SkippedNotScheduled);
        }

        // launchd's StartCalendarInterval catches up after the Mac was off or asleep.
        // Priming now would open a window at the wrong time and shift every later
        // boundary, which is worse than skipping.
        let due = anchor.local_on(date)?;
        let late_by = (now - due).num_minutes();
        if late_by > cfg.grace_minutes && cfg.on_missed == OnMissed::Skip {
            let mut rec = RunRecord::new(&args.anchor, Outcome::MissedTooStale);
            rec.scheduled_for = Some(due);
            rec.late_by_minutes = Some(late_by);
            state::append_run(&rec)?;
            eprintln!(
                "{} — {} ({}m late, grace {}m); nothing spent",
                anchor.label(),
                Outcome::MissedTooStale.label(),
                late_by,
                cfg.grace_minutes
            );
            notify(cfg, Outcome::MissedTooStale, &format!("{} missed by {}m", anchor.label(), late_by))?;
            return Ok(Outcome::MissedTooStale);
        }
    }

    if args.dry_run {
        let mut rec = RunRecord::new(&args.anchor, Outcome::DryRun);
        rec.scheduled_for = scheduled.and_then(|a| a.local_on(config::today_edt()).ok());
        println!("would run (cwd {}):\n  {}", config::prime_cwd()?.display(), render_command(cfg));
        state::append_run(&rec)?;
        return Ok(Outcome::DryRun);
    }

    let cwd = config::prime_cwd()?;
    std::fs::create_dir_all(&cwd)?;

    let started = std::time::Instant::now();
    let out = Command::new(&cfg.claude_bin)
        .args(build_args(cfg))
        .envs(build_env())
        .current_dir(&cwd)
        .output()
        .with_context(|| format!("could not execute {} — is the path still correct?", cfg.claude_bin))?;
    let elapsed = started.elapsed().as_millis() as u64;

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let parsed: Option<serde_json::Value> = serde_json::from_str(stdout.trim()).ok();

    let is_error = !out.status.success()
        || parsed
            .as_ref()
            .and_then(|v| v.get("is_error"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

    let mut rec = RunRecord::new(&args.anchor, if is_error { Outcome::Error } else { Outcome::Ok });
    rec.duration_ms = Some(elapsed);
    rec.scheduled_for = scheduled.and_then(|a| a.local_on(config::today_edt()).ok());
    if let Some(v) = &parsed {
        rec.cost_usd = v.get("total_cost_usd").and_then(|c| c.as_f64());
        rec.session_id = v.get("session_id").and_then(|s| s.as_str()).map(str::to_string);
    }

    if is_error {
        let detail = parsed
            .as_ref()
            .and_then(|v| v.get("result"))
            .and_then(|r| r.as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                let s = stderr.trim();
                if s.is_empty() { stdout.trim().to_string() } else { s.to_string() }
            });
        rec.error = Some(truncate(&detail, 500));
        state::append_run(&rec)?;
        eprintln!("{} — error: {}", args.anchor, rec.error.as_deref().unwrap_or(""));
        notify(cfg, Outcome::Error, &format!("{} failed", args.anchor))?;
        return Ok(Outcome::Error);
    }

    state::append_run(&rec)?;
    println!(
        "{} — ok in {}ms{}",
        args.anchor,
        elapsed,
        rec.cost_usd.map(|c| format!(" (${c:.6})")).unwrap_or_default()
    );
    notify(cfg, Outcome::Ok, &format!("{} primed", args.anchor))?;
    Ok(Outcome::Ok)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

fn notify(cfg: &Config, outcome: Outcome, message: &str) -> Result<()> {
    let should = match cfg.notify_on {
        NotifyOn::Never => false,
        NotifyOn::Always => true,
        NotifyOn::Failure => !matches!(outcome, Outcome::Ok | Outcome::DryRun),
    };
    if !should {
        return Ok(());
    }
    let script = format!(
        r#"display notification {} with title "claude-primer""#,
        applescript_string(message)
    );
    // Best-effort: a notification failure must never fail the prime itself.
    let _ = Command::new("/usr/bin/osascript").arg("-e").arg(script).output();
    Ok(())
}

fn applescript_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', r"\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config { claude_bin: "/Users/x/.local/bin/claude".into(), ..Default::default() }
    }

    #[test]
    fn args_carry_every_cost_control() {
        let a = build_args(&cfg());
        let joined = a.join(" ");
        assert!(joined.contains("--system-prompt"));
        assert!(joined.contains("--strict-mcp-config"));
        assert!(joined.contains(r#"{"mcpServers":{}}"#));
        assert!(joined.contains(r#"{"effortLevel":"low"}"#));
        assert!(joined.contains("--max-turns 1"));
        assert_eq!(a[0], "-p");
    }

    #[test]
    fn tool_definitions_are_stripped() {
        // Measured: the tool schemas were 11,129 of the 11,139 input tokens a prime
        // sent. `--tools ""` removes them from the model's context entirely.
        let a = build_args(&cfg());
        let i = a.iter().position(|x| x == "--tools").expect("--tools must be passed");
        assert_eq!(a[i + 1], "", "--tools must be given an empty list");
    }

    #[test]
    fn prompt_caching_is_disabled() {
        // Primes are 5h apart and the cache TTL is 1h, so every prime paid a
        // cache-write premium it could never read back.
        let env = build_env();
        assert!(env.iter().any(|(k, v)| *k == "CLAUDE_CODE_DISABLE_PROMPT_CACHING" && *v == "1"));
    }

    #[test]
    fn bare_is_never_passed() {
        // --bare would refuse OAuth and CLAUDE_CODE_OAUTH_TOKEN, billing the API
        // instead of the subscription window.
        assert!(!build_args(&cfg()).iter().any(|a| a == "--bare"));
    }

    #[test]
    fn model_comes_from_config() {
        let mut c = cfg();
        c.model = "sonnet".into();
        let a = build_args(&c);
        let i = a.iter().position(|x| x == "--model").unwrap();
        assert_eq!(a[i + 1], "sonnet");
    }

    #[test]
    fn rendered_command_quotes_json_payloads_and_shows_env() {
        let rendered = render_command(&cfg());
        assert!(rendered.starts_with("CLAUDE_CODE_DISABLE_PROMPT_CACHING=1 "));
        assert!(rendered.contains("/Users/x/.local/bin/claude "));
        assert!(rendered.contains(r#"'{"mcpServers":{}}'"#));
        assert!(rendered.contains("'Reply with exactly: OK'"));
        // An empty --tools value must survive quoting as a visible ''.
        assert!(rendered.contains("--tools ''"));
    }

    #[test]
    fn shell_quoting_handles_embedded_quotes() {
        assert_eq!(shell_quote("plain"), "plain");
        assert_eq!(shell_quote("/abs/path"), "/abs/path");
        assert_eq!(shell_quote("two words"), "'two words'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn applescript_strings_are_escaped() {
        assert_eq!(applescript_string(r#"a "b""#), r#""a \"b\"""#);
    }

    #[test]
    fn truncation_is_lossless_below_the_cap() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }

    fn at(h: u32, m: u32) -> chrono::DateTime<Local> {
        use chrono::TimeZone;
        let d = config::today_edt().and_hms_opt(h, m, 0).unwrap();
        config::edt().from_local_datetime(&d).unwrap().with_timezone(&Local)
    }

    #[test]
    fn auto_resolves_to_the_anchor_that_just_fired() {
        let c = cfg();
        assert_eq!(resolve_auto(&c, at(10, 30)).unwrap().label(), "10:30");
        assert_eq!(resolve_auto(&c, at(15, 30)).unwrap().label(), "15:30");
    }

    #[test]
    fn auto_tolerates_launchd_firing_slightly_early() {
        // A hair before 10:30 is still the 10:30 anchor, not the 05:30 one.
        assert_eq!(resolve_auto(&cfg(), at(10, 29)).unwrap().label(), "10:30");
    }

    #[test]
    fn auto_attributes_a_catch_up_to_the_missed_anchor() {
        // Mac was off at 05:30 and booted at 08:00; the staleness guard must see 05:30.
        assert_eq!(resolve_auto(&cfg(), at(8, 0)).unwrap().label(), "05:30");
    }

    #[test]
    fn auto_before_the_first_anchor_falls_back_to_it() {
        assert_eq!(resolve_auto(&cfg(), at(2, 0)).unwrap().label(), "05:30");
    }
}
