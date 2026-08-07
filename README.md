# claude-primer

Aligns Claude Code's 5-hour usage windows to your workday.

```
cli/        the claude-primer CLI (Rust)
menubar/    an optional menu bar app (Swift)
```

## The problem

Claude Code's usage allowance runs on a **rolling 5-hour session window that starts on your first message** and expires exactly 5 hours later. It's anchored to the user, not the clock.

By default, that boundary lands whenver your first prompt of the day happens to fall. E.g. you start working at 9am and use up all your credits 2 hours in, but you still have 3 hours until your window ends at 2pm, limiting your work throughput.

## The fix

Choose where the boundaries land to optimize around your workday, by sending one trivial prompt at each intended window start:

```
prime 05:30  →  window 05:30 ────────── 10:30     # e.g. start working at 9am, window resets in 1.5 hours
prime 10:30  →  window 10:30 ────────── 15:30
prime 15:30  →  window 15:30 ────────── 20:30
```

Up to 5 near-free prompts laid out predictably over the workday, timed to maximize the number of daily usage limit windows to work around your sleep and eating schedules. `claude-primer` installs the launchd units and `pmset` wake events that make those primes fire on schedule, unattended, including while the Mac is asleep.


## Install

Requires macOS and a Claude Pro/Max/Team/Enterprise subscription.

```sh
make install                     # builds both, installs the binary and the app

claude setup-token               # browser login; copy the token it prints
claude-primer install            # paste the token, then one sudo prompt
claude-primer menubar enable     # optional, see "Menu bar app" below
```

`make install` puts the binary at `~/.local/bin/claude-primer` deliberately: `claude-primer install` **refuses to run from `cli/target/`**, because the launchd job stores the binary's absolute path permanently and a build-directory path would break on the next `cargo clean`.

Other targets: `make cli`, `make menubar`, `make test`, `make uninstall`, `make clean`.

`install` will:

1. Resolve the absolute path to your `claude` binary (launchd jobs get a minimal `PATH`, so this must be absolute).
2. Ask for a token from `claude setup-token` — a one-year OAuth token. It's stored in the LaunchAgent plist at mode `0600` and never leaves your machine. This is what lets a 05:30 prime work with the screen locked, since it bypasses the keychain.
3. Write and bootstrap `~/Library/LaunchAgents/com.claude-primer.agent.plist`.
4. With one `sudo` prompt, write and bootstrap `/Library/LaunchDaemons/com.claude-primer.wake.plist`, which arms the wake events.

Install once. **Nothing needs restarting after a reboot** — launchd reloads both units automatically at login/boot, and `pmset` events persist in system power-management preferences.

### Uninstall

```sh
claude-primer uninstall
```

Unloads both launchd jobs (`launchctl bootout`) and cancels the `pmset` wake events that `claude-primer` armed — matching each one exactly, so your Mac's own scheduled wakes (calendar alarms, macOS analytics) are left alone. `pmset schedule cancelall` would wipe those too, so it is never used. Your config and logs are kept.

## Is it safe?

Short version: it sends a two-word prompt on a timer, and everything it touches is reversible with one command.

- **It only ever sends one trivial message.** The whole conversation is `-p "ok"`, answered under a replaced system prompt of `"Reply with exactly: OK"` — the replacement is there to drop Claude Code's real system prompt, which is most of the token cost. It cannot read your files, run commands, or edit anything: `--tools ""` removes every tool from the model's context, so there is nothing for it to act with.
- **It runs in an empty directory.** No project `CLAUDE.md`, hooks, or `.mcp.json` are loaded, and its transcripts stay out of your real projects' `--resume` history.
- **Your token stays on this machine.** Stored in a `0600` LaunchAgent plist, read only by the prime. Nothing is uploaded anywhere.
- **It never touches other apps' settings.** It writes only its own launchd plists and its own files under `~/.config` and `~/.local/share`. `pmset` events are cancelled by exact match, so your calendar alarms survive.
- **It cannot spend money.** On a subscription a prime draws from your usage quota; the dollar figures in the logs are what the request *would* cost at API rates, not a bill.
- **The menu bar app is read-only.** It renders `claude-primer snapshot` and nothing else. Deleting it does not affect whether primes fire.
- **`claude-primer uninstall` removes all of it.** Both launchd jobs and its own wake events. `make uninstall` also removes the app.

The only risks surround *scheduling*, not safety: a prime landing at a time that opens nothing, or an anchor missed because the Mac was off. Neither can damage anything outside this tool's own context.

---

## Verifying it works

A prime is a headless `claude -p` subprocess that lives about two seconds. There is **no visible UI** — no window, no dock icon, no notification, no sound. If you're at the Mac when one fires, you'll see nothing. So here's where to actually look.

### 1. `claude-primer status` — the intended audit trail

Rolls up everything below into one screen: anchors and next fire times, recent runs with outcome and cost, whether both launchd units are loaded, which wake events are armed, the current window estimate, and a warning as your one-year token nears expiry.

### 2. `runs.jsonl` — what actually fired

```sh
tail -5 ~/.local/share/claude-primer/runs.jsonl
```

One line per invocation. Statuses you'll see:

| Status | Meaning |
|---|---|
| `ok` | prime landed and **opened a new window** |
| `wasted: window already open` | the call succeeded but opened nothing — window was already running and doesn't reset the window properly, might want to re-optimize your schedule |
| `missed: too stale` | anchor passed while the Mac was off; skipped deliberately (see below) |
| `error` | the `claude` call failed — check `stderr` in the same record |

### 3. Are the launchd units loaded?

```sh
launchctl list | grep claude-primer
```

```
-    0    com.claude-primer.agent
```

Columns are `PID`, `Status`, `Label`:

- `-` under PID means **loaded but not executing right now** — the normal resting state.
- `Status` is the **last exit code**. `0` = the last prime succeeded. Non-zero = it failed.
- **Absent from the list entirely** = not loaded. This is the failure you actually want to catch.

### 4. Are the wake events armed?

```sh
pmset -g sched
```

You should see one repeating event for the first anchor and one-time `wake` events owned by `claude-primer` for the later ones. Your pre-existing system events (calendar alarms, analytics) must still be listed — `claude-primer` cancels only its own events by exact match and never calls `cancelall`.

### 5. `/usage` — the actual goal

In any interactive Claude Code session, run `/usage`. The 5-hour window should report as starting at your prime time. This is the ground truth that the whole tool exists to move.

### 6. The session transcript

Each prime writes a real transcript to `~/.claude/projects/<encoded-empty-dir>/<uuid>.jsonl`. Because primes run with cwd pinned to a dedicated empty directory, they're filed under their own project folder and stay out of the `--resume` / `--continue` history of your real projects.

---

## Missed anchors

launchd's `StartCalendarInterval` **catches up**: if a scheduled time passes while the Mac is off or asleep, the job runs at the next opportunity. Left unguarded that's actively harmful here — shut down at 22:00, boot at 08:00, and the missed 05:30 prime fires at 08:00, opening a window 08:00–13:00 and knocking your whole grid off its anchors.

So `run` checks staleness first:

- **`on_missed = "skip"`** (default) — more than `grace_minutes` past the anchor's true time, log `missed: too stale` and spend nothing. The grid stays put and the next on-time anchor resumes it.
- **`on_missed = "shift"`** — prime anyway and accept the shifted window. Useful if you boot mid-morning and want *a* window immediately.

Anchors that fall while the Mac is fully shut down are simply missed. With FileVault on, a cold boot halts at the pre-boot unlock screen — nothing is decrypted and no user session exists, so no LaunchAgent runs. Sleep is fine; shutdown is not recoverable without a second always-on machine.

---

## Menu bar app

Optional. A small Swift app showing the Claude mark in the menu bar, with the schedule behind it.

```sh
make install
claude-primer menubar enable      # starts it now and at every login
claude-primer menubar disable     # reverses it; the app stays installed
```

```
┌──────────────────────────┐
│ Window ends  19:54       │
│ Next prime   Fri 05:30   │
│ Today        ✓ 3/4       │
│ ──────────────────────── │
│ 05:30  ok                │
│ 10:30  ok                │
│ 15:30  ok                │
│ ──────────────────────── │
│ Prime now…               │
│ Open status in Terminal  │
│ Edit config…             │
│ Reveal logs in Finder    │
│ ──────────────────────── │
│ Launch at login       ✓  │
│ Quit                     │
└──────────────────────────┘
```

The menu bar shows **just the mark** when everything is fine. A `⚠` appears beside it when a prime was skipped as stale, and a `✗` when the agent isn't loaded or a prime failed — so a problem is visible without opening anything, and a healthy setup is silent.

**"Prime now…" asks for confirmation.** It's the only action here that spends anything: it starts a 5-hour window beginning immediately, which shifts the rest of the day's schedule.

### It is not load-bearing

The app is a **viewer, never a participant.** launchd fires the primes. If it crashes, is quit, is never launched, or you delete it, every prime still fires identically — there is no code path where it can prevent, delay, or corrupt one.

It holds no window arithmetic either. All state comes from `claude-primer snapshot`, so the CLI stays the single source of truth:

```sh
claude-primer snapshot | jq .
```

Severity (`health`) is computed in Rust, so the menu bar and `status` can't disagree about what counts as a problem. It reads local files only — zero tokens, zero network, and it never invokes `claude`.

### Building it

`swiftc` against the Command Line Tools SDK. **Full Xcode is not required** — the bundle is assembled by `make menubar` and ad-hoc signed, which is enough for a locally built app (no quarantine flag, so Gatekeeper doesn't prompt).

`LSUIElement` is set, so there's no Dock icon and nothing in the app switcher. Launch-at-login uses a LaunchAgent rather than `SMAppService`, because the bundle is ad-hoc signed and `SMAppService`'s behaviour depends on signing identity while launchd's does not. `menubar enable` terminates any hand-launched copy first, since launchd execs the binary directly and bypasses the usual single-instance dedup.

---

## Configuration

The config lives **outside the repo**, in a hidden directory in your home folder, so it won't appear in your editor's file tree:

```
~/.config/claude-primer/config.toml
```

```sh
code $(claude-primer config-path)     # open it
claude-primer status                  # also prints the path at the top
```

```toml
claude_bin    = "/Users/you/.local/bin/claude"   # resolved at install
anchors       = ["05:30", "10:30", "15:30"]      # the base set
weekdays      = ["Mon", "Tue", "Wed", "Thu", "Fri"]
model         = "haiku"
timezone      = "local"                          # this Mac's clock; macOS handles DST
notify_on     = "failure"                        # "failure" | "never" | "always"
on_missed     = "skip"                           # "skip" | "shift"
grace_minutes = 20
boundary_wait_secs = 300                         # wait out a closing window; 0 disables
```

After editing, **re-run `claude-primer install`** to regenerate the launchd units — anchor times are baked into the plist as `StartCalendarInterval` entries. `status` reflects an edit immediately, but the live schedule won't until you reinstall.

### Waiting out a closing window

Anchors spaced near 5 hours apart are fragile on their own: a prime takes 2–11 seconds and
launchd can fire a moment early, so anchor N+1 lands just *inside* the window anchor N
opened — and one second of drift would otherwise waste every remaining prime that day.

So a scheduled prime that finds a window closing shortly will sleep until it does:

```
11:24 — window closes at 11:24:21; waiting 15s so this prime opens a new one
11:24 — ok in 11687ms
```

`boundary_wait_secs` (default 300) caps the wait. Two things bound it further:

- **It never waits past your grace budget.** Waiting must not produce a window the staleness
  guard had already judged too late to open, so the wait is capped by whatever remains of
  `grace_minutes`. A prime that's already 18 minutes late gets at most 2 more.
- **Manual primes never wait.** "Prime now" and `--force` mean *now*; sleeping under a button
  press would be surprising. Those report the wasted outcome instead.

It aims a few seconds past the expiry rather than exactly at it, because the expiry is this
tool's own local estimate and the server's view may differ slightly. Set
`boundary_wait_secs = 0` to switch waiting off entirely.

### Setting times per day

Every day of the week can have its own schedule — different times, and a different *number* of primes. There is nothing weekend-specific about it.

Two ways to write times, and they work together:

| | What it does |
|---|---|
| `anchors` + `weekdays` | **Shorthand** — "these times, on these days." So you don't repeat the same three times five times over. |
| `[schedules]` | **Per-day** — names one day and gives it its own times. Overrides the shorthand for that day. |

**Same times every workday** (the default). Shorthand only:

```toml
anchors  = ["05:30", "10:30", "15:30"]
weekdays = ["Mon", "Tue", "Wed", "Thu", "Fri"]
```

**Workdays plus different weekend times.** Shorthand for Mon–Fri, overrides for the rest:

```toml
anchors  = ["05:30", "10:30", "15:30"]
weekdays = ["Mon", "Tue", "Wed", "Thu", "Fri"]

[schedules]
Sat = ["09:00", "14:00"]
Sun = ["11:00"]
```

**Every day different.** Skip the shorthand entirely — leave `anchors` and `weekdays` empty:

```toml
anchors  = []
weekdays = []

# ex. my personal config
[schedules]
Mon = ["0:30", "05:30", "10:30", "15:30", "20:30"]
Tue = ["05:30", "10:30", "15:30", "20:30"]
Wed = ["05:30", "10:30", "15:30", "20:30"]
Thu = ["05:30", "10:30", "15:30", "20:30"]
Fri = ["05:30", "10:30", "15:30"]
Sat = ["08:30", "13:30", "18:30"]
Sun = ["09:30", "14:30", "19:30"]
```

`status` renders whichever you use as a full week:

```
  schedule
    Mon  05:30 → 06:30   10:30 → 11:30   15:30 → 16:30   (per-day override)
    Thu  07:00 → 08:00   12:00 → 13:00                   (per-day override)
    Sun  11:00 → 12:00                                   (per-day override)
```

Notes:

- A day in `[schedules]` is active **whether or not** it appears in `weekdays`.
- An empty list (`Wed = []`) switches that day off.
- A day in neither is off — no `StartCalendarInterval` entry is written for it, so nothing fires. By default that's Saturday and Sunday.
- Omitting `[schedules]` entirely keeps the original behaviour, so existing configs are unaffected.

One consequence worth knowing: `pmset` allows only **one** repeating wake event, and it goes to the earliest anchor of the `anchors`/`weekdays` shorthand. Anchors from `[schedules]` are covered by rolling one-time wake events instead, re-armed daily by the root daemon. Without that daemon, those primes only fire when the Mac is already awake.

`timezone = "local"` (the default) reads every time as **this Mac's own clock**. A fixed offset is also accepted if you'd rather pin times to an absolute zone: `EDT`, `EST`, `UTC`, or an explicit `UTC-4` / `UTC+5:30`. These do **not** shift with DST.

---

## Cost of a prime

Each prime is deliberately minimal:

```
CLAUDE_CODE_DISABLE_PROMPT_CACHING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
claude -p "ok" --model haiku --system-prompt "Reply with exactly: OK" --tools "" \
  --max-turns 1 --output-format json --settings '{"effortLevel":"low"}' \
  --strict-mcp-config --mcp-config '{"mcpServers":{}}'
```

Prime only spends your quote, so read the dollar figures below as a proxy for size along with the input tokens (which is what matters).

### Measured

| | reported cost | input tokens |
|---|---|---|
| `--system-prompt` + `--model haiku` only | $0.022613 | 11,139 |
| full flag set above | **$0.000675** | **240** |

**46× fewer tokens against your cap.**

Two findings drove that gap:

- **Tool definitions were 11,129 of the 11,139 input tokens.** Replacing the system prompt isn't enough; Claude Code still ships the full schemas for Read, Write, Bash, Grep, Task and the rest. A prime needs no tools at all, so `--tools ""` removes them from the model's context entirely. This was the whole cost.
- **Prompt caching was actively counterproductive.** Every prime wrote ~11k tokens into a 1-hour cache it could never read back, because primes are five hours apart. It paid the cache-*write* premium on every single run for zero benefit. `CLAUDE_CODE_DISABLE_PROMPT_CACHING=1` removes the charge.

The rest: `--system-prompt` replaces the default system prompt (measured: the prompt itself drops to ~10 tokens), `--strict-mcp-config` with an empty config stops your global MCP servers loading, `--settings` overrides a global `effortLevel`, `--model haiku` keeps the call off the Sonnet-specific weekly cap, and cwd is pinned to an empty directory so no project `CLAUDE.md`, hooks, or `.mcp.json` are discovered.

`--bare` would be the obvious choice here and is deliberately **not** used: bare mode never reads OAuth credentials or `CLAUDE_CODE_OAUTH_TOKEN`, so it would bill the API instead of touching your subscription window — the opposite of the goal.