mod config;
mod launchd;
mod pmset;
mod prime;
mod snapshot;
mod state;
mod statusline;
mod window;

use anyhow::{anyhow, Context, Result};
use chrono::Local;
use clap::{Parser, Subcommand};
use config::{Config, AGENT_LABEL, DAEMON_LABEL};

#[derive(Parser)]
#[command(
    name = "claude-primer",
    about = "Align Claude Code's 5-hour usage windows to your workday",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write and load the launchd units, and arm the wake events.
    Install {
        /// A token from `claude setup-token`. Prompted for if omitted.
        #[arg(long)]
        token: Option<String>,
        /// Skip registering the Claude Code status line.
        #[arg(long)]
        no_statusline: bool,
    },
    /// Send one priming prompt. This is what the LaunchAgent invokes.
    Run {
        /// An anchor like 05:30, or `auto` to resolve it from the clock.
        #[arg(long, default_value = "auto")]
        anchor: String,
        /// Compose the command and log it without spending anything.
        #[arg(long)]
        dry_run: bool,
        /// Bypass the weekday and staleness guards.
        #[arg(long)]
        force: bool,
    },
    /// Show the schedule, recent runs, unit health, and the current window.
    Status,
    /// Print the path to the config file, for `code $(claude-primer config-path)`.
    ConfigPath,
    /// One-line readout for Claude Code's status line. Local state only, zero tokens.
    Statusline,
    /// Print full state as JSON. The contract the menu bar app renders.
    Snapshot,
    /// Manage the menu bar app's launch-at-login agent.
    Menubar {
        #[command(subcommand)]
        action: MenubarAction,
    },
    /// Re-arm the rolling wake events. Run as root by the LaunchDaemon.
    ArmWakes,
    /// Unload the units and cancel our wake events. Config and logs are kept.
    Uninstall,
}

#[derive(Subcommand)]
enum MenubarAction {
    /// Start the app now and at every login.
    Enable,
    /// Stop it and remove the login agent. The app itself is left installed.
    Disable,
    /// Report whether the login agent is loaded, as JSON.
    Status,
}

fn main() {
    restore_default_sigpipe();
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Rust ignores SIGPIPE at startup, which turns `claude-primer status | head` into a
/// panic on a broken pipe instead of a quiet exit. Restore the default disposition so
/// this behaves like any other CLI when its output is truncated.
fn restore_default_sigpipe() {
    // SAFETY: setting a signal disposition to SIG_DFL before any threads are spawned.
    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

const SIGPIPE: i32 = 13;
const SIG_DFL: usize = 0;

extern "C" {
    fn signal(sig: i32, handler: usize) -> usize;
}

fn real_main() -> Result<()> {
    match Cli::parse().command {
        Cmd::Install { token, no_statusline } => cmd_install(token, no_statusline),
        Cmd::Run { anchor, dry_run, force } => {
            let cfg = Config::load()?;
            prime::run(&cfg, prime::PrimeArgs { anchor, dry_run, force })?;
            Ok(())
        }
        Cmd::Status => cmd_status(),
        Cmd::ConfigPath => {
            println!("{}", Config::path()?.display());
            Ok(())
        }
        Cmd::Statusline => cmd_statusline(),
        Cmd::Snapshot => cmd_snapshot(),
        Cmd::Menubar { action } => cmd_menubar(action),
        Cmd::ArmWakes => cmd_arm_wakes(),
        Cmd::Uninstall => cmd_uninstall(),
    }
}

fn cmd_install(token: Option<String>, no_statusline: bool) -> Result<()> {
    // Create a commented starter config only if none exists. An existing config is
    // read and never rewritten, so hand-added comments survive reinstalls.
    if Config::write_default_if_absent(&resolve_claude_bin()?)? {
        println!("wrote {}", Config::path()?.display());
    }

    let cfg = Config::load()?;

    if cfg.claude_bin.is_empty() || !std::path::Path::new(&cfg.claude_bin).exists() {
        return Err(anyhow!(
            "claude_bin in {} points at something that doesn't exist:\n  {:?}\n\n\
             Edit it by hand — this file is never rewritten automatically, so your\n\
             comments are safe. Try: {}",
            Config::path()?.display(),
            cfg.claude_bin,
            resolve_claude_bin().unwrap_or_else(|_| "(claude not found)".into())
        ));
    }
    println!("claude binary: {}", cfg.claude_bin);

    let exe = std::env::current_exe()?.canonicalize()?.display().to_string();

    // The plist stores this path forever. A build-directory path breaks the moment
    // the repo is moved or `cargo clean` runs, and the job then fails silently.
    if exe.contains("/target/release/") || exe.contains("/target/debug/") {
        return Err(anyhow!(
            "refusing to install from a build directory:\n  {exe}\n\n\
             The launchd job stores this path permanently, and it would break on the\n\
             next `cargo clean` or if you move the repo. Install the binary first:\n\n\
             \x20 mkdir -p ~/.local/bin\n\
             \x20 cp {exe} ~/.local/bin/claude-primer\n\
             \x20 claude-primer install"
        ));
    }

    // Reuse the installed token when one exists, so reinstalling to apply a schedule
    // change doesn't silently downgrade auth to the keychain.
    let token = match token {
        Some(t) => Some(t),
        None => match launchd::existing_token() {
            Some(t) => {
                println!("reusing the token already installed (pass --token to replace it)");
                Some(t)
            }
            None => prompt_token()?,
        },
    };
    if token.is_none() {
        eprintln!(
            "note: no token supplied — primes will fall back to the login keychain, \
             which is not available when the screen is locked or after a cold boot."
        );
    }

    std::fs::create_dir_all(config::prime_cwd()?)?;

    let agent = launchd::write_agent(&cfg, &exe, token.as_deref())?;
    launchd::bootstrap_agent(&agent)?;
    println!("LaunchAgent: {} (loaded)", agent.display());

    install_daemon(&exe)?;

    if !no_statusline {
        register_statusline(&exe)?;
    }

    println!("\nRun `claude-primer status` to confirm. Nothing needs restarting after a reboot.");
    Ok(())
}

fn install_daemon(exe: &str) -> Result<()> {
    let xml = launchd::daemon_plist_xml(exe)?;
    let tmp = std::env::temp_dir().join(format!("{DAEMON_LABEL}.plist"));
    std::fs::write(&tmp, &xml)?;
    let dest = launchd::daemon_plist_path();

    println!("\nThe wake-arming daemon needs root (pmset requires it, and a LaunchAgent");
    println!("cannot sudo unattended). You'll be asked for your password once.");

    let script = format!(
        "cp {tmp} {dest} && chown root:wheel {dest} && chmod 644 {dest} && \
         launchctl bootout system/{label} 2>/dev/null; launchctl bootstrap system {dest}",
        tmp = tmp.display(),
        dest = dest.display(),
        label = DAEMON_LABEL
    );
    let status = std::process::Command::new("/usr/bin/sudo")
        .args(["/bin/sh", "-c", &script])
        .status()
        .context("could not run sudo")?;
    let _ = std::fs::remove_file(&tmp);

    if status.success() {
        println!("LaunchDaemon: {} (loaded)", dest.display());
    } else {
        eprintln!(
            "warning: the daemon did not install. Later anchors will only wake the Mac\n\
             if it happens to be awake already. Re-run install, or arm manually with\n\
             `sudo claude-primer arm-wakes`."
        );
    }
    Ok(())
}

fn register_statusline(exe: &str) -> Result<()> {
    let path = config::home()?.join(".claude/settings.json");
    let mut settings: serde_json::Value = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("{} is not valid JSON", path.display()))?
    } else {
        serde_json::json!({})
    };

    if settings.get("statusLine").is_some() {
        println!("\nsettings.json already has a statusLine — leaving it alone.");
        println!("To use this one instead, set command to: {exe} statusline");
        return Ok(());
    }

    if !confirm(&format!("\nRegister the status line in {}?", path.display()))? {
        return Ok(());
    }

    settings["statusLine"] = serde_json::json!({
        "type": "command",
        "command": format!("{exe} statusline"),
        "refreshInterval": 30
    });
    std::fs::write(&path, serde_json::to_string_pretty(&settings)? + "\n")?;
    println!("status line registered (local subprocess, zero tokens)");
    Ok(())
}

fn cmd_arm_wakes() -> Result<()> {
    let cfg = Config::load()?;
    if !pmset::is_root() {
        return Err(anyhow!("arm-wakes needs root — try `sudo claude-primer arm-wakes`"));
    }
    let ledger = pmset::arm(&cfg)?;
    println!(
        "armed {} one-time wake(s); repeating slot at {} on {}",
        ledger.armed.len(),
        ledger.repeat_time_local.as_deref().unwrap_or("-"),
        ledger.repeat_weekdays.as_deref().unwrap_or("-")
    );
    Ok(())
}

fn cmd_status() -> Result<()> {
    let cfg = Config::load()?;
    let now = Local::now();

    println!("claude-primer");
    // Printed first because the config lives outside any project directory, so it is
    // otherwise easy to miss.
    println!("  config          {}", Config::path()?.display());
    println!("  logs            {}", config::runs_log()?.display());
    println!("  claude binary   {}", cfg.claude_bin);
    println!("  model           {}", cfg.model);
    println!("  on missed       {:?} (grace {}m)", cfg.on_missed, cfg.grace_minutes);

    // Under SystemLocal the configured times are already what the Mac's clock reads,
    // so one column says everything. A fixed offset needs both, since launchd fires
    // in the system zone and the two differ.
    let mode = cfg.mode()?;
    let sys_offset = now.offset().local_minus_utc() / 3600;
    match mode {
        config::TimeMode::SystemLocal => {
            println!("\n  schedule (this Mac's clock, UTC{sys_offset:+})")
        }
        config::TimeMode::Fixed(_) => println!(
            "\n  schedule ({} declared → system-local UTC{sys_offset:+})",
            cfg.timezone
        ),
    }
    let today = cfg.today()?;
    for (weekday, anchors) in cfg.active_days()? {
        let probe = config::nearest_date_with_weekday(today, weekday);
        let rendered: Vec<String> = anchors
            .iter()
            .map(|a| match mode {
                config::TimeMode::SystemLocal => a.label(),
                config::TimeMode::Fixed(_) => a
                    .local_on(probe, mode)
                    .map(|l| format!("{} → {}", a.label(), l.format("%H:%M")))
                    .unwrap_or_else(|_| a.label()),
            })
            .collect();
        let overridden = cfg
            .schedules
            .keys()
            .any(|d| config::parse_weekday(d).map(|w| w == weekday).unwrap_or(false));
        println!(
            "    {:<4} {}{}",
            weekday.to_string(),
            rendered.join("   "),
            if overridden { "   (per-day override)" } else { "" }
        );
    }
    if cfg.active_days()?.is_empty() {
        println!("    nothing scheduled — check `weekdays` and `[schedules]`");
    }
    if matches!(mode, config::TimeMode::Fixed(_)) {
        println!(
            "    note: {:?} is a fixed offset and does not shift with daylight saving.",
            cfg.timezone
        );
    }

    println!("\n  next up");
    for (_, a, dt) in window::upcoming(&cfg, 14)?.into_iter().take(4) {
        // Under a fixed offset the configured label and the local firing time differ,
        // so show both. Under SystemLocal they are the same and one is enough.
        match mode {
            config::TimeMode::SystemLocal => println!("    {}", dt.format("%a %d %b %H:%M")),
            config::TimeMode::Fixed(_) => println!(
                "    {}   (declared {} {})",
                dt.format("%a %d %b %H:%M"),
                a.label(),
                cfg.timezone
            ),
        }
    }

    match state::last_window_start()? {
        Some(start) => {
            let ends = start + window::window_len();
            if ends > now {
                println!(
                    "\n  window          started {}, {} left (ends {})",
                    start.format("%H:%M"),
                    window::fmt_hm(ends - now),
                    ends.format("%H:%M")
                );
            } else {
                println!("\n  window          none open (last ended {})", ends.format("%a %H:%M"));
            }
        }
        None => println!("\n  window          none recorded yet"),
    }

    println!("\n  launchd");
    for (label, name) in [(AGENT_LABEL, "agent "), (DAEMON_LABEL, "daemon")] {
        let s = launchd::unit_status(label);
        let desc = if !s.loaded {
            "NOT LOADED".to_string()
        } else {
            let pid = s.pid.map(|p| format!("pid {p}")).unwrap_or_else(|| "idle".into());
            let exit = s.last_exit.map(|e| format!(", last exit {e}")).unwrap_or_default();
            format!("loaded, {pid}{exit}")
        };
        println!("    {name}  {desc}");
    }

    let events = pmset::scheduled_events();
    let mine = pmset::ours(&events);
    println!("\n  pmset           {} of ours armed, {} system events intact", mine.len(), events.len() - mine.len());
    for e in mine.iter().take(4) {
        println!("    {e}");
    }

    let runs = state::read_runs()?;
    println!("\n  recent runs");
    if runs.is_empty() {
        println!("    none yet");
    }
    for r in runs.iter().rev().take(6) {
        let cost = r.cost_usd.map(|c| format!("  ${c:.6}")).unwrap_or_default();
        let late = r.late_by_minutes.map(|m| format!("  ({m}m late)")).unwrap_or_default();
        println!(
            "    {}  {:<6}  {}{}{}",
            r.ts.format("%a %d %b %H:%M"),
            r.anchor,
            r.outcome.label(),
            late,
            cost
        );
    }
    Ok(())
}

fn cmd_snapshot() -> Result<()> {
    let cfg = Config::load()?;
    let snap = snapshot::build(&cfg, Local::now())?;
    println!("{}", serde_json::to_string_pretty(&snap)?);
    Ok(())
}

fn cmd_menubar(action: MenubarAction) -> Result<()> {
    match action {
        MenubarAction::Enable => {
            let app = config::menubar_app_path()?;
            let plist = launchd::write_menubar_agent(&app)?;
            // Terminate any instance started by hand first. launchd execs the binary
            // directly, which bypasses LaunchServices' usual single-instance dedup, so
            // without this you end up with two menu bar icons. Handled here rather than
            // by a self-terminating guard in Swift, which would fight KeepAlive and
            // restart-loop.
            launchd::terminate_menubar_instances();
            launchd::bootstrap_menubar(&plist)?;
            println!("menu bar app enabled — it will start at every login");
            println!("  app   {}", app.display());
            println!("  agent {}", plist.display());
        }
        MenubarAction::Disable => {
            let stopped = launchd::bootout_menubar()?;
            let plist = launchd::menubar_plist_path()?;
            let removed = std::fs::remove_file(&plist).is_ok();
            if stopped || removed {
                println!("menu bar app disabled (the app itself is still installed)");
            } else {
                println!("menu bar app was not enabled");
            }
        }
        MenubarAction::Status => {
            // JSON because the app itself reads this to render its own toggle state.
            let u = launchd::unit_status(config::MENUBAR_LABEL);
            println!(
                r#"{{"enabled":{},"app":{}}}"#,
                u.loaded,
                serde_json::to_string(&config::menubar_app_path()?.display().to_string())?
            );
        }
    }
    Ok(())
}

fn cmd_statusline() -> Result<()> {
    statusline::drain_stdin();
    // Never fail visibly in the middle of someone's prompt.
    let line = (|| -> Result<String> {
        let cfg = Config::load()?;
        Ok(statusline::render(&statusline::snapshot(&cfg, Local::now())?))
    })()
    .unwrap_or_else(|_| "⏱ claude-primer not configured".to_string());
    println!("{line}");
    Ok(())
}

fn cmd_uninstall() -> Result<()> {
    if launchd::bootout_agent()? {
        println!("agent unloaded");
    } else {
        println!("agent was not loaded");
    }
    if let Ok(p) = launchd::agent_plist_path() {
        let _ = std::fs::remove_file(p);
    }

    let dest = launchd::daemon_plist_path();
    if dest.exists() {
        println!("removing the root daemon (one sudo prompt)");
        let script = format!(
            "launchctl bootout system/{DAEMON_LABEL} 2>/dev/null; rm -f {}",
            dest.display()
        );
        let _ = std::process::Command::new("/usr/bin/sudo").args(["/bin/sh", "-c", &script]).status();
    }

    if pmset::is_root() {
        let n = pmset::disarm()?;
        println!("cancelled {n} wake event(s)");
    } else {
        let script = format!("{} arm-wakes --help >/dev/null 2>&1", std::env::current_exe()?.display());
        let _ = script;
        println!("run `sudo claude-primer uninstall` to also cancel armed wake events");
    }

    println!("config and logs retained");
    Ok(())
}

fn resolve_claude_bin() -> Result<String> {
    // launchd gives jobs a minimal PATH, so this must end up absolute.
    let candidates = [
        config::home()?.join(".local/bin/claude"),
        std::path::PathBuf::from("/opt/homebrew/bin/claude"),
        std::path::PathBuf::from("/usr/local/bin/claude"),
    ];
    for c in candidates {
        if c.exists() {
            return Ok(c.display().to_string());
        }
    }
    let out = std::process::Command::new("/usr/bin/which").arg("claude").output()?;
    let found = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !found.is_empty() && std::path::Path::new(&found).exists() {
        return Ok(found);
    }
    Err(anyhow!("could not find the `claude` binary — set claude_bin in config.toml"))
}

fn prompt_token() -> Result<Option<String>> {
    use std::io::Write;
    println!("\nA long-lived token from `claude setup-token` lets a prime run even when");
    println!("the login keychain is locked. It is stored in the LaunchAgent plist at");
    println!("mode 0600 and never leaves this machine. Leave blank to skip.");
    print!("token: ");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    let s = s.trim().to_string();
    Ok((!s.is_empty()).then_some(s))
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(matches!(s.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}
