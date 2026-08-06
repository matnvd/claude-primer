# claude-primer — align Claude Code's 5-hour usage windows to the workday

## Context

Claude Code's usage allowance is governed by a **rolling 5-hour session window that starts on your first message** and expires exactly 5 hours later — it is anchored to you, not to the clock. Left alone, the window boundary lands wherever your first prompt happened to fall, which routinely means hitting "limit reached, resets in 3 hours" in the middle of an afternoon.

The fix is to *choose* where the boundaries land by sending one trivial prompt at each intended window start. Priming at 05:30 → window runs 05:30–10:30; priming again at 10:30 → 10:30–15:30; again at 15:30 → 15:30–20:30. Three near-free prompts lay a predictable grid over the workday, and each new window is a fresh allowance.

`claude-primer` is a Rust CLI that installs the launchd units and `pmset` wake events to make those primes fire on schedule, unattended, including while the Mac is asleep.

**Set expectations honestly:** priming does not grant more quota. It controls *when* windows start. The secondary effect — that an early anchor can expose your workday to three window-allowances instead of two — is real but is bounded by the weekly cap, which on **Pro** is the tighter constraint. The `simulate` subcommand exists so anchors get chosen from arithmetic rather than guesswork.

**Scope boundary:** Mac-only.

- **Asleep** (lid closed or idle sleep) — RAM stays powered, the login session and keychain stay alive, launchd keeps running, and `pmset schedule wake` wakes it in time. **Works.**
- **Shut down or restarted** — the Mac cannot boot itself into a working state. With **FileVault On**, a cold boot halts at the pre-boot unlock screen: nothing is decrypted, no user session exists, and LaunchAgents only run inside a logged-in session. `pmset poweron` is separately unreliable on Apple Silicon. **Any anchor falling while the Mac is off is missed**, and cannot be recovered without a second always-on host.

Since this machine *is* shut down and restarted periodically, missed anchors are a routine condition, not an edge case — which is what the staleness guard under *Missed anchors* exists to handle. Nothing needs restarting by hand after a reboot; launchd reloads both units automatically.

---

## Decisions

| Decision | Choice |
|---|---|
| Language | Rust (single static binary, no runtime for launchd to depend on) |
| Host | Mac-only: LaunchAgent + `pmset` wake |
| Strategy | Fixed clock anchors (default `05:30`, `10:30`, `15:30` **EDT**, Mon–Fri) |
| Timezone | Fixed EDT (UTC-4), converted to system-local when writing plists; no auto-DST |
| Missed anchors | Skipped past a 20-minute grace, rather than fired late and misaligned |
| Visibility | Claude Code status line (0 tokens, local state only) + failure-only notifications; menu bar deferred |
| Auth | Local `CLAUDE_CODE_OAUTH_TOKEN` in the plist, mode `0600` |
| Wake arming | Root LaunchDaemon re-arms one-time wakes on a rolling horizon |

---

## Constraints that drive the design

1. **`pmset repeat` holds exactly one repeating wake.** From `man pmset`: *"you may only have one pair of repeating events scheduled — a 'power on' event and a 'power off' event."* One slot, three anchors. Therefore `pmset repeat wakeorpoweron` covers **anchor #1 only** (the overnight one that genuinely needs it); anchors #2–#3 use `pmset schedule wake`, which permits many one-time events but must be periodically re-armed. Hence the root daemon.

2. **`pmset schedule` / `repeat` require root.** A LaunchAgent cannot `sudo` unattended. Arming therefore lives in a LaunchDaemon, not the Agent.

3. **Never call `pmset schedule cancelall`.** The machine already has system wake events (calendar alarms, analytics). Cancel only our own events by exact `(type, datetime, owner)` match, using the owner string `claude-primer` and the datetimes persisted in state.

4. **`--bare` is disqualified.** It is the obvious choice for a scripted call, but the docs are explicit: *"In bare mode, Claude Code never reads OAuth credentials or the system keychain"* and *"Bare mode does not read `CLAUDE_CODE_OAUTH_TOKEN`."* It requires an `ANTHROPIC_API_KEY`, which bills the API instead of touching the subscription window — the exact opposite of the goal. Cheapness must be achieved by other means (see #5).

5. **The global config would make a naive prime expensive.** `~/.claude/settings.json` sets `"effortLevel": "max"` and enables a plugin, and global MCP servers (Canva, Gmail, Drive) would load on a normal `-p` run. Every prime must neutralize these explicitly.

6. **launchd gives jobs a minimal `PATH`** (`/usr/bin:/bin:/usr/sbin:/sbin`). `claude` lives at `~/.local/bin/claude`, which is not on it. The absolute path must be resolved at install time and written into config — the single most likely cause of a silently dead job.

7. **The keychain is why the OAuth token matters.** `CLAUDE_CODE_OAUTH_TOKEN` is precedence #5 and outranks the keychain login at #6, so putting it in the plist removes the keychain from the path entirely. A 05:30 prime then works with the screen locked. Generated once via `claude setup-token` (one-year lifetime — `status` warns as expiry nears).

---

## The prime invocation

The whole tool exists to run this line correctly, three times a day:

```
CLAUDE_CODE_DISABLE_PROMPT_CACHING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
<claude_bin> -p "ok"
  --model haiku
  --system-prompt "Reply with exactly: OK"
  --tools ""
  --max-turns 1
  --output-format json
  --settings '{"effortLevel":"low"}'
  --strict-mcp-config --mcp-config '{"mcpServers":{}}'
```

Run with **cwd set to a dedicated empty directory** under the tool's data dir, so no project `CLAUDE.md`, hooks, or `.mcp.json` are discovered.

**Measured, and the gap is larger than anticipated during planning:**

| | reported cost | input tokens |
|---|---|---|
| `--system-prompt` + `--model haiku` only | $0.022613 | 11,139 |
| full flag set above | **$0.000675** | **240** |

The dollar column is a size proxy only. On a subscription, `total_cost_usd` reports what the request *would* cost at API rates; nothing is billed. A prime spends **quota** — the 5-hour session allowance and the weekly caps — so the input-token column is the operative one.

Two findings, both discovered by inspecting the prime's own transcript after the first real run:

- **Tool definitions dominated.** 11,129 of the 11,139 input tokens were the built-in tool schemas. Replacing the system prompt is not sufficient — Claude Code still ships the full schemas. `--tools ""` removes them from the model's context entirely, and a prime needs no tools.
- **Prompt caching was counterproductive.** Every prime wrote ~11k tokens into a 1-hour cache it could never read back, since primes are five hours apart. It paid the cache-*write* premium every run for zero benefit. Disabling it removes the charge outright.

Parse the JSON result for `is_error`, `total_cost_usd`, `usage`, and `session_id`; append every run to a JSONL log.

---

## How a prime is triggered, and what you'll see

launchd spawns a **separate headless `claude -p` process**. It does not type into, resume, or otherwise touch any interactive Claude Code session you have open — it opens a brand-new session, runs a single turn, prints JSON to stdout, and exits in a few seconds.

There is **no visible UI**: no window, no dock icon, no notification, no sound.

It is auditable in four places:

| Where | What it shows |
|---|---|
| `claude-primer status` / `runs.jsonl` | timestamp, cost, ok/error — the intended audit trail |
| `~/.claude/projects/<encoded-empty-dir>/<uuid>.jsonl` | a real session transcript, filed under its own project folder |
| `/usage` in any interactive session | the 5-hour window reported as starting at the prime time — the actual goal |
| claude.ai account activity | the request, like any other |

The second row is a direct reason for pinning cwd to a dedicated empty directory: Claude Code files transcripts by working directory, so primes stay out of the `--resume` / `--continue` history of real projects.

`notify_on = "failure"` posts a macOS notification via `osascript -e 'display notification …'` **only** when a prime errors or is missed. Silence means it worked.

### Always-on readout: the Claude Code status line

Claude Code spawns the configured command as a **local subprocess**, pipes it session JSON on stdin, and renders its stdout as a bar at the bottom of the TUI:

```jsonc
"statusLine": {
  "type": "command",
  "command": "claude-primer statusline",
  "refreshInterval": 30
}
```

```
~/GitHub/bios-db  main*
⏱ 2h14m left in window · next prime 15:30 · ✓ 3/3 today
```

States: `✓` all primes landed · `⚠` a prime was skipped as stale · `✗` agent not loaded or last exit non-zero · `—` outside the workday envelope.

> **Hard constraint: `statusline` must never invoke `claude`.** The lazy way to learn window state would be to shell out and read `/usage`. That would cost tokens *and* — far worse — **start a new 5-hour window every 30 seconds**, destroying exactly what this tool exists to control. `statusline` reads local state only: `runs.jsonl`, config, and cached `launchctl`/`pmset` output. Pure file I/O and date arithmetic, zero network, zero tokens.

**Menu bar: deferred.** A menu bar item requires a resident process (a SwiftBar plugin or a small AppKit `NSStatusItem` app). Ship the CLI and status line first and confirm the schedule holds for a week. `statusline` is designed to feed either host, so adding one later is a second output format (`--format=swiftbar`), not rework.

---

## Layout

```
claude-primer/
  Cargo.toml
  src/
    main.rs        clap CLI: install | run | status | simulate | arm-wakes | uninstall
    config.rs      TOML config + serde types
    state.rs       run log (JSONL) + armed-wake ledger
    prime.rs       builds & spawns the claude invocation, parses JSON result
    statusline.rs  one-line readout from local state; never touches the network
    launchd.rs     LaunchAgent + LaunchDaemon plist generation via the `plist` crate
    pmset.rs       schedule / repeat / precise cancel wrappers
    window.rs      5-hour window arithmetic; powers `simulate` and `status`
```

Crates: `clap` (derive), `serde` + `serde_json` + `toml`, `chrono` + `chrono-tz`, `plist`, `anyhow`.

Config at `~/.config/claude-primer/config.toml`; state and logs at `~/.local/share/claude-primer/`.

```toml
claude_bin    = "/Users/<you>/.local/bin/claude"   # resolved at install, not assumed
anchors       = ["05:30", "10:30", "15:30"]
weekdays      = ["Mon", "Tue", "Wed", "Thu", "Fri"]
model         = "haiku"
timezone      = "EDT"                              # fixed UTC-4; no automatic DST shifting
notify_on     = "failure"                          # "failure" | "never" | "always"
on_missed     = "skip"                             # "skip" | "shift"
grace_minutes = 20
```

### Timezone handling

Anchors are declared in **EDT (fixed UTC-4)**. No automatic DST adjustment — offsets are handled manually.

This needs explicit conversion because **`StartCalendarInterval` has no timezone field**: launchd fires it in whatever the *system* timezone is. If the system reports `Atlantic/Bermuda` (UTC-3), one hour off EDT, then a `05:30` EDT anchor must be written into the plist as `06:30` system-local, and `pmset schedule` datetimes converted the same way.

`status` displays every anchor in **both** EDT and system-local time, and warns when the system timezone changes — otherwise a laptop that moves between zones would silently re-point every anchor.

### Missed anchors (the shutdown case)

launchd's `StartCalendarInterval` **catches up**: if a scheduled time passes while the Mac is off or asleep, the job runs at the next opportunity. Left unguarded that is actively harmful — shutting down at 22:00 and booting at 08:00 would fire the missed 05:30 prime at 08:00, opening a window 08:00–13:00 and knocking the whole grid off its intended anchors.

`run` therefore checks staleness before priming:

- **`skip` (default)** — more than `grace_minutes` past the anchor's true time, log `missed: too stale` and exit without spending anything. The grid stays on its anchors and the next on-time anchor resumes it.
- **`shift`** — prime anyway and accept the shifted window. Useful if you boot mid-morning and want *a* window immediately.

Reboots need no intervention: LaunchAgents in `~/Library/LaunchAgents/` load automatically at every login, the LaunchDaemon in `/Library/LaunchDaemons/` at every boot, and `pmset` events persist in system power-management preferences. Install once.

---

## Subcommands

**`install`** — resolve `claude` absolute path; prompt for the `claude setup-token` value; write the LaunchAgent plist to `~/Library/LaunchAgents/com.claude-primer.agent.plist` at mode `0600` with `StartCalendarInterval` as an *array* of dicts (one per anchor) and the token in `EnvironmentVariables`; `launchctl bootstrap gui/$(id -u)` it. Then, with one sudo prompt: write the LaunchDaemon to `/Library/LaunchDaemons/com.claude-primer.wake.plist`, bootstrap it, and run `arm-wakes` once.

Use `launchctl bootstrap` / `bootout`, not the deprecated `load` / `unload`.

**`run --anchor <name>`** — what the Agent invokes. Skip and log if today is not an enabled weekday. Otherwise execute the prime, parse the result, append to `runs.jsonl`, update the window estimate. Supports `--dry-run` to print the command without spending quota.

**`arm-wakes`** — root-only, invoked daily by the Daemon. Sets `pmset repeat wakeorpoweron <anchor#1> MTWRF`, then arms `pmset schedule wake` for anchors #2–#3 across the next 7 days. Cancels only previously-armed events recorded in the ledger, by exact match.

**`status`** — anchors and next fire time for each; last N runs with outcome and cost; whether the Agent and Daemon are loaded (`launchctl print`); which of our wake events are currently armed (`pmset -g sched`); the estimated current window (last successful prime + 5h); and a warning as the one-year token nears expiry.

**`statusline`** — prints the single-line readout and exits. Registered in `~/.claude/settings.json` by `install` (with confirmation, since it edits a file the tool doesn't own). Must remain fast and offline: no `claude` invocation, no network.

**`simulate --workday 09:00-17:00`** — pure arithmetic, no API calls. Renders the window grid a given anchor set produces and reports how many window-allowances the workday touches, so anchors can be tuned deliberately. This is the piece that makes the tool an optimizer rather than a cron wrapper.

**`uninstall`** — `bootout` both units, cancel our armed wake events precisely, retain config and logs.

---

## Verification

1. `cargo build --release`, then `claude-primer simulate --workday 09:00-17:00` — validates the window math with zero quota spent.
2. `claude-primer run --anchor test --dry-run` — confirm the composed command line, especially the absolute `claude` path and the neutralizing flags.
3. `claude-primer run --anchor test` for real — expect a fast `OK` and a `total_cost_usd` in `runs.jsonl` that is a small fraction of an unoptimized `-p` call. Then confirm in an interactive session that `/usage` shows a window that started just now.
4. `claude-primer install`, then set a throwaway anchor 3 minutes out, close the lid, and confirm the Mac wakes and the run lands in `runs.jsonl` on time. **This is the test that matters** — it exercises pmset wake, launchd firing, the minimal `PATH`, and token auth with a locked screen all at once.
5. Verify `pmset -g sched` still lists the pre-existing system wake events, proving cancellation was precise and did not nuke calendar alarms.
6. `claude-primer status` — Agent and Daemon both loaded, wakes armed, window reported, and each anchor shown in **both** EDT and system-local time.
7. **Staleness guard:** set an anchor to a time already 30+ minutes in the past and invoke `run` directly. It must log `missed: too stale` and spend nothing — *not* fire a misaligned prime. Then set `on_missed = "shift"` and confirm the opposite.
8. **Reboot persistence:** restart the Mac, log in, and run `claude-primer status` without touching anything. Both units must report loaded and the wake events still armed. If an anchor fell while the Mac was off, `runs.jsonl` should show it skipped rather than fired late.
9. **Status line costs nothing:** confirm `claude-primer statusline` makes no network calls, then check `/usage` is unchanged after leaving Claude Code open for an hour with `refreshInterval: 30` (≈120 invocations, which must cost exactly zero tokens and must not have started a window).

---

## Note on policy

Anthropic states that advertised Pro/Max limits *"assume ordinary, individual usage of Claude Code and the Agent SDK."* A handful of tiny scheduled prompts per day, on your own account, to time your own windows, sits inside that. `claude setup-token` is documented for exactly this kind of scripted use. Keep the volume as designed (3/day) and don't scale it up.
