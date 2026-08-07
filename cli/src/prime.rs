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
    let date = cfg.today()?;
    let mode = cfg.mode()?;
    // Today's own anchor set, so a weekend override resolves against its own times.
    let mut anchors = cfg.anchors_for(date)?;
    if anchors.is_empty() {
        anchors = cfg.anchors()?;
    }
    anchors.sort();

    let mut best: Option<Anchor> = None;
    for a in &anchors {
        let due = a.local_on(date, mode)?;
        if (now - due).num_minutes() >= -EARLY_TOLERANCE_MINS {
            best = Some(*a);
        }
    }
    // Fired before the first anchor of the day: attribute it to that first anchor.
    best.or_else(|| anchors.first().copied())
        .ok_or_else(|| anyhow::anyhow!("no anchors configured"))
}

/// Seconds past the window's expiry to aim for. The expiry is our own local estimate,
/// so landing exactly on it risks the server still considering the window open.
const BOUNDARY_OVERSHOOT_SECS: i64 = 10;

/// How long to wait for an open window to expire, or `None` to not wait at all.
///
/// Bounded by two things. `boundary_wait_secs` caps how long a scheduled job may sit
/// blocked. The remaining grace budget caps it too — waiting must never push a prime
/// past the staleness threshold it just passed, or the tool would produce a window it
/// had already decided was too late to open.
fn boundary_wait(
    cfg: &Config,
    now: chrono::DateTime<Local>,
    until: chrono::DateTime<Local>,
    scheduled: Option<Anchor>,
    forced: bool,
) -> Option<std::time::Duration> {
    // A manual prime means "now". Silently sleeping under a button press would be
    // surprising, so those report the wasted outcome instead.
    if forced || cfg.boundary_wait_secs == 0 {
        return None;
    }
    let anchor = scheduled?;
    let due = anchor
        .local_on(cfg.today().ok()?, cfg.mode().ok()?)
        .ok()?;

    // Nothing to wait for. Checked before adding the overshoot, or an already-expired
    // window would still produce a pointless sleep.
    let remaining = (until - now).num_seconds();
    if remaining <= 0 {
        return None;
    }
    let need = remaining + BOUNDARY_OVERSHOOT_SECS;

    let already_late = (now - due).num_seconds().max(0);
    let grace_left = cfg.grace_minutes * 60 - already_late;
    let budget = cfg.boundary_wait_secs.min(grace_left.max(0));

    (need <= budget).then(|| std::time::Duration::from_secs(need as u64))
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
        let date = cfg.today()?;
        let mode = cfg.mode()?;

        // No weekday check here on purpose. The installed plist only carries entries
        // for scheduled days, and the one case it could not cover — a job firing on a
        // day removed from the config but not yet reinstalled — is the user's to fix by
        // reinstalling. Every other route to an unscheduled day (manual runs, "Prime
        // now") sets --force, and launchd catch-up is already handled by the staleness
        // guard below.
        //
        // launchd's StartCalendarInterval catches up after the Mac was off or asleep.
        // Priming now would open a window at the wrong time and shift every later
        // boundary, which is worse than skipping.
        let due = anchor.local_on(date, mode)?;
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
        rec.scheduled_for = scheduled.and_then(|a| cfg.today().ok().zip(cfg.mode().ok()).and_then(|(d, m)| a.local_on(d, m).ok()));
        println!("would run (cwd {}):\n  {}", config::prime_cwd()?.display(), render_command(cfg));
        state::append_run(&rec)?;
        return Ok(Outcome::DryRun);
    }

    let cwd = config::prime_cwd()?;
    std::fs::create_dir_all(&cwd)?;

    // Sample the window *before* running: the run appends to the same log this reads,
    // so afterwards it can no longer tell whether a window was already in flight.
    //
    // A prime that lands inside an open window opens nothing — it only spends quota
    // from the window already running. Anchors spaced exactly 5h apart make that easy
    // to hit, since a few seconds of drift is enough.
    let mut window_open_until = state::last_window_start()?
        .map(|start| start + crate::window::window_len())
        .filter(|end| *end > now);

    // If that window is about to expire, wait it out rather than burning the prime.
    // Anchors spaced exactly 5h apart put every later prime a few seconds inside the
    // previous window, so without this a one-second drift wastes the whole rest of the
    // day's schedule.
    if let Some(until) = window_open_until {
        if let Some(wait) = boundary_wait(cfg, now, until, scheduled, args.force) {
            println!(
                "{} — window closes at {}; waiting {}s so this prime opens a new one",
                args.anchor,
                until.format("%H:%M:%S"),
                wait.as_secs()
            );
            std::thread::sleep(wait);
            // Re-sample: the wait was calculated to outlast the window, so this should
            // now be None. Reading it again rather than assuming keeps the outcome
            // honest if anything opened a window while we slept.
            let after = Local::now();
            window_open_until = state::last_window_start()?
                .map(|start| start + crate::window::window_len())
                .filter(|end| *end > after);
        }
    }

    // The window opens when the server processes the request, which is close to when
    // the call starts — not when it returns 2–11s later. `ts` is what
    // `last_window_start` uses as the window's origin, so stamping it after the call
    // would push every countdown that far into the future and claim time you don't
    // have. Taken here, before spawning, it errs a shade early instead.
    let call_started_at = Local::now();
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

    // A successful call is not the same as an opened window. Reporting `ok` here when
    // nothing opened would reset every countdown in the tool to a full 5 hours.
    let outcome = match (is_error, window_open_until) {
        (true, _) => Outcome::Error,
        (false, Some(_)) => Outcome::OkWindowAlreadyOpen,
        (false, None) => Outcome::Ok,
    };

    let mut rec = RunRecord::new(&args.anchor, outcome);
    rec.ts = call_started_at;
    rec.window_open_until = window_open_until;
    rec.duration_ms = Some(elapsed);
    rec.scheduled_for = scheduled.and_then(|a| cfg.today().ok().zip(cfg.mode().ok()).and_then(|(d, m)| a.local_on(d, m).ok()));
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
    let cost = rec.cost_usd.map(|c| format!(" (${c:.6})")).unwrap_or_default();

    if let Some(until) = window_open_until {
        // Say plainly that nothing opened. This is the case where a cheerful "ok" was
        // actively misleading — quota was spent and the schedule did not move.
        println!(
            "{} — no new window: one was already open until {} ({} remaining){}",
            args.anchor,
            until.format("%H:%M"),
            crate::window::fmt_hm(until - now),
            cost
        );
        println!("  this anchor sits inside the previous window — space it further apart");
        notify(
            cfg,
            Outcome::OkWindowAlreadyOpen,
            &format!("{} opened nothing — window already ran to {}", args.anchor, until.format("%H:%M")),
        )?;
        return Ok(Outcome::OkWindowAlreadyOpen);
    }

    println!("{} — ok in {}ms{}", args.anchor, elapsed, cost);
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

    /// A wall-clock time today on this Mac, which is exactly how anchors are read
    /// under the default `timezone = "local"`.
    fn at(h: u32, m: u32) -> chrono::DateTime<Local> {
        use chrono::TimeZone;
        let d = Local::now().date_naive().and_hms_opt(h, m, 0).unwrap();
        Local.from_local_datetime(&d).earliest().unwrap()
    }

    fn secs(d: Option<std::time::Duration>) -> Option<u64> { d.map(|x| x.as_secs()) }

    #[test]
    fn waits_out_a_window_that_is_about_to_close() {
        // The whole point: a one-second drift must not waste the prime.
        let c = cfg();
        let now = at(10, 30);
        let until = now + chrono::Duration::seconds(3);
        let w = boundary_wait(&c, now, until, Anchor::parse("10:30").ok(), false);
        // 3s of window + the overshoot that guards against clock skew.
        assert_eq!(secs(w), Some(3 + BOUNDARY_OVERSHOOT_SECS as u64));
    }

    #[test]
    fn does_not_wait_out_a_window_with_hours_left() {
        // A 00:30 anchor sitting an hour inside the previous window is a schedule
        // mistake, not drift. Blocking the job for an hour would be worse than saying so.
        let c = cfg();
        let now = at(10, 30);
        let until = now + chrono::Duration::hours(1);
        assert_eq!(boundary_wait(&c, now, until, Anchor::parse("10:30").ok(), false), None);
    }

    #[test]
    fn never_waits_past_the_grace_budget() {
        // Waiting must not produce a window the staleness guard had already judged too
        // late to open. With 20m grace and 18m already lost, only 2m of budget remain.
        let c = cfg();
        let now = at(10, 48);
        let until = now + chrono::Duration::minutes(4);
        assert_eq!(boundary_wait(&c, now, until, Anchor::parse("10:30").ok(), false), None);

        // The same window, reached on time, is comfortably affordable.
        let ontime = at(10, 30);
        let until2 = ontime + chrono::Duration::minutes(4);
        assert!(boundary_wait(&c, ontime, until2, Anchor::parse("10:30").ok(), false).is_some());
    }

    #[test]
    fn a_forced_prime_never_waits() {
        // "Prime now" means now. Sleeping under a button press would be surprising.
        let c = cfg();
        let now = at(10, 30);
        let until = now + chrono::Duration::seconds(3);
        assert_eq!(boundary_wait(&c, now, until, Anchor::parse("10:30").ok(), true), None);
    }

    #[test]
    fn waiting_can_be_switched_off() {
        let mut c = cfg();
        c.boundary_wait_secs = 0;
        let now = at(10, 30);
        let until = now + chrono::Duration::seconds(3);
        assert_eq!(boundary_wait(&c, now, until, Anchor::parse("10:30").ok(), false), None);
    }

    #[test]
    fn an_already_expired_window_needs_no_wait() {
        let c = cfg();
        let now = at(10, 30);
        assert_eq!(boundary_wait(&c, now, now - chrono::Duration::seconds(1), Anchor::parse("10:30").ok(), false), None);
    }

    #[test]
    fn the_window_timestamp_is_taken_before_the_call() {
        // `ts` is the origin every countdown is measured from. Stamping it after the
        // call would silently add the call's 2-11s to every window the tool reports.
        let src = include_str!("prime.rs");
        let impl_src = src.split("#[cfg(test)]").next().unwrap();
        let stamped = impl_src.find("let call_started_at").expect("timestamp must be captured");
        let spawned = impl_src.find("Command::new(&cfg.claude_bin)").expect("call must exist");
        assert!(stamped < spawned, "the window timestamp must be taken before the call");
        assert!(impl_src.contains("rec.ts = call_started_at"), "and it must be the one recorded");
    }

    #[test]
    fn a_minute_of_drift_does_not_cascade() {
        // The scenario: 05:30 fires a minute late, so its window runs to 10:31 and the
        // 10:30 anchor lands inside it. That must resolve to a short wait, not a wasted
        // prime — otherwise one minute of drift would cost every later prime that day.
        let c = cfg();
        let fires = at(10, 30);
        let window_until = at(10, 31);
        let w = boundary_wait(&c, fires, window_until, Anchor::parse("10:30").ok(), false)
            .expect("a minute of drift must be waited out, not wasted");
        assert_eq!(w.as_secs(), 60 + BOUNDARY_OVERSHOOT_SECS as u64);
    }

    #[test]
    fn drift_beyond_the_wait_budget_is_reported_not_waited_out() {
        // Past boundary_wait_secs the job would be blocked too long to be worth it, so
        // the prime runs and reports `wasted` instead. With the 300s default, the
        // breaking point is about five minutes of accumulated drift.
        let c = cfg(); // boundary_wait_secs = 300
        let fires = at(10, 30);

        // 4m59s of drift: still inside the budget once the overshoot is added.
        let ok = boundary_wait(&c, fires, fires + chrono::Duration::seconds(289), Anchor::parse("10:30").ok(), false);
        assert!(ok.is_some(), "just under the budget should still wait");

        // 5m01s: over the budget, so no wait.
        let too_far = boundary_wait(&c, fires, fires + chrono::Duration::seconds(301), Anchor::parse("10:30").ok(), false);
        assert!(too_far.is_none(), "past the budget it should report rather than block");
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
