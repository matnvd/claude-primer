# claude-primer

Aligns Claude Code's 5-hour usage windows to your workday.

```
cli/        the claude-primer CLI (Rust)
menubar/    an optional menu bar app (Swift)
```

## The problem

Claude Code's usage allowance runs on a **rolling 5-hour session window that starts on your first message** and expires exactly 5 hours later. It's anchored to you, not to the clock.

Left alone, that boundary lands wherever your first prompt of the day happened to fall. Send a throwaway message at 07:12 checking something, and your window now ends at 12:12 — so you hit "limit reached, resets in 3 hours" at 12:12, mid-task, and the next window ends at some equally arbitrary time. The boundary walks around your day.

## The fix

Choose where the boundaries land, by sending one trivial prompt at each intended window start:

```
prime 05:30  →  window 05:30 ────────── 10:30
prime 10:30  →  window 10:30 ────────── 15:30
prime 15:30  →  window 15:30 ────────── 20:30
```

Three near-free prompts lay a predictable grid over the workday, and each new window is a fresh allowance. `claude-primer` installs the launchd units and `pmset` wake events that make those primes fire on schedule, unattended, including while the Mac is asleep.


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
5. Optionally register the status line in `~/.claude/settings.json` (asks first).

Install once. **Nothing needs restarting after a reboot** — launchd reloads both units automatically at login/boot, and `pmset` events persist in system power-management preferences.

### Uninstall

```sh
claude-primer uninstall
```

Unloads both launchd jobs (`launchctl bootout`) and cancels the `pmset` wake events that `claude-primer` armed — matching each one exactly, so your Mac's own scheduled wakes (calendar alarms, macOS analytics) are left alone. `pmset schedule cancelall` would wipe those too, so it is never used. Your config and logs are kept.

## Is it safe?

Short version: it sends a two-word prompt on a timer, and everything it touches is reversible with one command.

- **It only ever sends one trivial prompt.** `-p "ok"` with an empty tool set. It cannot read your files, run commands, or edit anything — `--tools ""` removes every tool from the model's context, so there is nothing for it to act with.
- **It runs in an empty directory.** No project `CLAUDE.md`, hooks, or `.mcp.json` are loaded, and its transcripts stay out of your real projects' `--resume` history.
- **Your token stays on this machine.** Stored in a `0600` LaunchAgent plist, read only by the prime. Nothing is uploaded anywhere.
- **It never touches other apps' settings.** The one shared file it edits is `~/.claude/settings.json`, to register the status line, and it asks first. `pmset` events are cancelled by exact match so your calendar alarms survive.
- **It cannot spend money.** On a subscription a prime draws from your usage quota; the dollar figures in the logs are what the request *would* cost at API rates, not a bill.
- **The menu bar app is read-only.** It renders `claude-primer snapshot` and nothing else. Deleting it does not affect whether primes fire.
- **`claude-primer uninstall` removes all of it.** Both launchd jobs and its own wake events. `make uninstall` also removes the app.

The honest risks are all about *scheduling*, not safety: a prime landing at a time that opens nothing, or an anchor missed because the Mac was off. Neither can damage anything outside this tool's own state.

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
| `wasted: window already open` | the call succeeded but opened nothing — window was already running and doesn't reset the window properly, might want to add buffer time between anchors |
| `missed: too stale` | anchor passed while the Mac was off; skipped deliberately (see below) |
| `skipped: not a scheduled weekday` | working as configured |
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

## Status line

Register `claude-primer` as Claude Code's status line for an always-visible readout:

```jsonc
// ~/.claude/settings.json
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

`✓` all primes landed · `⚠` a prime was skipped as stale · `✗` agent not loaded or last exit non-zero · `—` outside the workday envelope.

**This costs zero tokens.** Claude Code runs the status line as a *local subprocess* — it pipes session JSON to stdin and renders stdout. No model is invoked and no network call happens. `claude-primer statusline` only reads local state and does date arithmetic.

> By design, `statusline` never invokes `claude`. Reading window state by shelling out to `/usage` would cost tokens *and*, far worse, **start a new 5-hour window every 30 seconds** — destroying the exact thing this tool exists to control.

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

Severity (`health`) is computed in Rust, so the menu bar and `status` can't disagree about what counts as a problem. Like the status line, it reads local files only — zero tokens, zero network, and it never invokes `claude`.

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
```

After editing, **re-run `claude-primer install`** to regenerate the launchd units — anchor times are baked into the plist as `StartCalendarInterval` entries. `status` reflects an edit immediately, but the live schedule won't until you reinstall.

> **Space your anchors more than 5 hours apart.**
>
> A prime opens a window that runs for exactly 5 hours. Any anchor that falls inside a
> still-open window spends quota and opens nothing — the window doesn't extend, and no new
> one starts until you next send a message.
>
> Gaps of exactly `5h00m` are the trap: a prime takes 2–11 seconds and launchd can fire a
> moment early, so the next anchor lands just inside the previous window. Leave a few minutes
> of slack (`05:30, 10:35, 15:40`). **Watch the wrap past midnight too** — a `20:30` anchor
> runs to `01:30`, so a `00:30` anchor the next morning is wasted every single day.
>
> When it happens, `runs.jsonl` records `wasted: window already open` with the time it
> collided with, and the status line shows `⚠`.

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

[schedules]
Mon = ["05:30", "10:30", "15:30"]
Tue = ["06:00", "11:00", "16:00"]
Wed = ["05:30", "10:30", "15:30"]
Thu = ["07:00", "12:00"]
Fri = ["05:30", "10:30"]
Sat = ["09:00", "14:00"]
Sun = ["11:00"]
```

`status` renders whichever you use as a full week:

```
  schedule (EDT declared → system-local UTC-3)
    Mon  05:30 → 06:30   10:30 → 11:30   15:30 → 16:30   (per-day override)
    Thu  07:00 → 08:00   12:00 → 13:00                   (per-day override)
    Sun  11:00 → 12:00                                   (per-day override)
```

Notes:

- A day in `[schedules]` is active **whether or not** it appears in `weekdays`.
- An empty list (`Wed = []`) switches that day off.
- A day in neither is off. By default that's Saturday and Sunday, and a run then exits with `skipped: not a scheduled weekday`, spending nothing.
- Omitting `[schedules]` entirely keeps the original behaviour, so existing configs are unaffected.

One consequence worth knowing: `pmset` allows only **one** repeating wake event, and it goes to the earliest anchor of the `anchors`/`weekdays` shorthand. Anchors from `[schedules]` are covered by rolling one-time wake events instead, re-armed daily by the root daemon. Without that daemon, those primes only fire when the Mac is already awake.

### Timezone

`timezone = "local"` (the default) reads every time as **this Mac's own clock**. `05:30` in the config means 05:30 on the menu-bar clock, no conversion, and **macOS handles daylight saving** — so the times don't drift across a DST transition.

A fixed offset is also accepted if you'd rather pin times to an absolute zone: `EDT`, `EST`, `UTC`, or an explicit `UTC-4` / `UTC+5:30`. These do **not** shift with DST, so a time declared in summer fires an hour off once the local zone leaves daylight saving. `status` labels the mode and, for a fixed offset, shows both the declared time and the local time it actually fires at.

This distinction exists because `StartCalendarInterval` has no timezone field — launchd always fires in the *system* zone, so anything that isn't already system-local has to be converted before it reaches the plist.

---

## Cost of a prime

Each prime is deliberately minimal:

```
CLAUDE_CODE_DISABLE_PROMPT_CACHING=1 CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
claude -p "ok" --model haiku --system-prompt "Reply with exactly: OK" --tools "" \
  --max-turns 1 --output-format json --settings '{"effortLevel":"low"}' \
  --strict-mcp-config --mcp-config '{"mcpServers":{}}'
```

### What "cost" means here

On a Pro or Max subscription **a prime does not cost you money.** The `total_cost_usd` that Claude Code reports — and that `claude-primer` logs — is what the request *would* cost at standard API rates. It's a usage meter, not a bill.

What a prime actually spends is **quota**: your 5-hour session allowance and your weekly caps. Dollars only enter the picture if you have separately enabled extra usage / usage credits *and* have gone past your included limits, at which point Claude continues at consumption-based pricing instead of blocking.

So read the dollar figures below as a proxy for size. **The input-token column is the one that matters**, because tokens are what your weekly cap is denominated in.

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

At 3 primes a day, 5 days a week, the scheduling overhead is roughly **3,600 input tokens per week** against your caps — and no money on a subscription.

---

## My Personal Config

claude_bin = "/Users/matnvd/.local/bin/claude"
anchors  = []
weekdays = []
model = "haiku"
timezone = "local"
notify_on = "failure"
on_missed = "skip"
grace_minutes = 20

[schedules]
Mon = ["05:30", "10:30", "15:30","20:30"]
Tue = ["05:30", "10:30", "15:30","20:30"]
Wed = ["05:30", "10:30", "15:30","20:30"]
Thu = ["05:30", "10:30", "15:30","20:30"]
Fri = ["05:30", "10:30", "15:30"] # friday nights off :)
Sat = ["08:30", "13:30", "18:30"]
Sun = ["08:30", "13:30", "18:30"]