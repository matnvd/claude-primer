// ClaudePrimer — menu bar readout for claude-primer.
//
// This app is a *viewer*, never a participant. launchd fires the primes; if this
// process crashes, is quit, or is never launched, every prime still fires identically.
// There is no code path here that can prevent, delay, or corrupt one.
//
// It deliberately contains no window arithmetic. All state comes from
// `claude-primer snapshot`, so the CLI stays the single source of truth — duplicating
// the 5-hour logic here would guarantee the two eventually disagree, and the CLI is
// the side with tests.

import AppKit
// Carbon, purely for RegisterEventHotKey. The AppKit alternative,
// NSEvent.addGlobalMonitorForEvents, requires Accessibility permission — it can observe
// *every* keystroke, so macOS gates it behind a prompt. RegisterEventHotKey registers
// one combination with the window server and receives nothing else, so it needs no
// permission and raises no dialog.
import Carbon.HIToolbox

// MARK: - The snapshot contract (mirrors cli/src/snapshot.rs)

struct Snapshot: Decodable {
    let window: WindowState
    let nextPrime: Prime?
    let today: Today
    let usage: UsageInfo?
    let recent: [RunSummary]
    let upcoming: [Prime]
    let units: Units
    let health: Health
    let paths: Paths

    enum CodingKeys: String, CodingKey {
        case window, today, usage, recent, upcoming, units, health, paths
        case nextPrime = "next_prime"
    }
}

struct WindowState: Decodable {
    let open: Bool
    let startedAt: Date?
    let endsAt: Date?
    let remainingSecs: Int?

    enum CodingKeys: String, CodingKey {
        case open
        case startedAt = "started_at"
        case endsAt = "ends_at"
        case remainingSecs = "remaining_secs"
    }
}

struct Prime: Decodable {
    let at: Date
    let anchor: String
}

struct Today: Decodable {
    let scheduled: Bool
    let expected: Int
    let done: Int
    let hadStaleMiss: Bool

    enum CodingKeys: String, CodingKey {
        case scheduled, expected, done
        case hadStaleMiss = "had_stale_miss"
    }
}

struct RunSummary: Decodable {
    let ts: Date
    let anchor: String
    let outcome: String
    let label: String
    let costUsd: Double?

    /// This run's own outcome, not the day's health — a single prime can have failed on
    /// a day that is otherwise fine, and vice versa.
    var marker: String {
        switch outcome {
        case "ok": return "✓"
        case "error": return "✗"
        default: return "⚠"   // wasted, missed_too_stale
        }
    }

    enum CodingKeys: String, CodingKey {
        case ts, anchor, outcome, label
        case costUsd = "cost_usd"
    }
}

/// Real usage from Claude Code's `/usage`. Absent unless the snapshot was taken with
/// --usage, which the menu does only when opened.
struct UsageInfo: Decodable {
    let sessionPct: Int?
    let sessionResetsAt: Date?
    let weekPct: Int?
    let weekResetsAt: Date?

    enum CodingKeys: String, CodingKey {
        case sessionPct = "session_pct"
        case sessionResetsAt = "session_resets_at"
        case weekPct = "week_pct"
        case weekResetsAt = "week_resets_at"
    }
}

struct Units: Decodable {
    let agent: UnitState
    let daemon: UnitState
}

struct UnitState: Decodable {
    let loaded: Bool
    let lastExit: Int?

    enum CodingKeys: String, CodingKey {
        case loaded
        case lastExit = "last_exit"
    }
}

/// Severity is decided in Rust, once. This side only renders it.
///
/// The menu bar icon stays the Claude mark in every state, so health is carried by a
/// prefix on the title instead — a changing icon reads as a different app, and users
/// find their menu bar item by its shape.
enum Health: String, Decodable {
    case ok, warn, error

    /// Appended after the icon. Empty when healthy, so a working setup is just the
    /// mark and nothing else.
    var prefix: String {
        switch self {
        case .ok: return ""
        case .warn: return " ⚠"
        case .error: return " ✗"
        }
    }
}

// MARK: - Icon

enum Icon {
    /// A drawn rendition of Claude's radiating burst mark.
    ///
    /// Built as a **template image** (`isTemplate = true`) so macOS tints it for the
    /// light and dark menu bar automatically — the reason this is a vector drawn in
    /// code rather than a bundled PNG, which would need two assets and still not match
    /// the accent-colour and reduced-transparency settings.
    static let claude: NSImage = {
        // 18pt is the standard menu bar icon size; AppKit re-runs this drawing block at
        // 2x on Retina, so the vector stays crisp.
        let side: CGFloat = 18
        let img = NSImage(size: NSSize(width: side, height: side), flipped: false) { rect in
            let c = CGPoint(x: rect.midX, y: rect.midY)
            let R = rect.width

            // Each ray is a petal — pointed at both ends, widest partway out — rather
            // than a triangle meeting at the centre. Triangles fuse into a solid hub at
            // small sizes and read as a sparkle; leaving a gap at the centre keeps the
            // individual rays distinguishable at 18pt.
            let count = 12
            let inner = R * 0.05     // gap at the centre
            let outer = R * 0.46
            let halfWidth = R * 0.055
            let bulge: CGFloat = 0.45 // where along the ray it is widest

            NSColor.black.setFill()

            for i in 0..<count {
                let angle = (CGFloat(i) / CGFloat(count)) * .pi * 2 - .pi / 2
                let dir = CGPoint(x: cos(angle), y: sin(angle))
                let normal = CGPoint(x: -dir.y, y: dir.x)

                let start = CGPoint(x: c.x + dir.x * inner, y: c.y + dir.y * inner)
                let end = CGPoint(x: c.x + dir.x * outer, y: c.y + dir.y * outer)
                let waistR = inner + (outer - inner) * bulge
                let waist = CGPoint(x: c.x + dir.x * waistR, y: c.y + dir.y * waistR)

                let p = NSBezierPath()
                p.move(to: start)
                p.curve(to: end,
                        controlPoint1: CGPoint(x: waist.x + normal.x * halfWidth,
                                               y: waist.y + normal.y * halfWidth),
                        controlPoint2: CGPoint(x: waist.x + normal.x * halfWidth,
                                               y: waist.y + normal.y * halfWidth))
                p.curve(to: start,
                        controlPoint1: CGPoint(x: waist.x - normal.x * halfWidth,
                                               y: waist.y - normal.y * halfWidth),
                        controlPoint2: CGPoint(x: waist.x - normal.x * halfWidth,
                                               y: waist.y - normal.y * halfWidth))
                p.close()
                p.fill()
            }
            return true
        }
        img.isTemplate = true
        return img
    }()

    /// Shown when the CLI can't be reached, so a broken setup is visibly different
    /// from a working one at a glance.
    static let unavailable: NSImage = {
        let img = NSImage(systemSymbolName: "questionmark.circle",
                          accessibilityDescription: "claude-primer unavailable")
            ?? claude
        img.isTemplate = true
        return img
    }()
}

struct Paths: Decodable {
    let config: String
    let runsLog: String

    enum CodingKeys: String, CodingKey {
        case config
        case runsLog = "runs_log"
    }
}

// MARK: - Running the CLI

enum CLI {
    static var binary: String {
        NSHomeDirectory() + "/.local/bin/claude-primer"
    }

    /// Runs a subcommand and returns stdout, or nil on any failure. Never throws into
    /// the UI: a missing or broken binary must degrade to a readable menu, not a crash.
    @discardableResult
    static func run(_ args: [String]) -> String? {
        guard FileManager.default.isExecutableFile(atPath: binary) else { return nil }
        let p = Process()
        p.executableURL = URL(fileURLWithPath: binary)
        p.arguments = args
        let out = Pipe()
        p.standardOutput = out
        p.standardError = Pipe()
        do {
            try p.run()
        } catch {
            return nil
        }
        let data = out.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        guard p.terminationStatus == 0 else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func snapshot(withUsage: Bool = false) -> Snapshot? {
        let args = withUsage ? ["snapshot", "--usage"] : ["snapshot"]
        guard let json = run(args), let data = json.data(using: .utf8) else { return nil }
        let dec = JSONDecoder()
        dec.dateDecodingStrategy = .custom { decoder in
            let s = try decoder.singleValueContainer().decode(String.self)
            // chrono emits RFC 3339 with fractional seconds; ISO8601DateFormatter needs
            // to be told about them explicitly or it returns nil.
            let withFrac = ISO8601DateFormatter()
            withFrac.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
            if let d = withFrac.date(from: s) { return d }
            let plain = ISO8601DateFormatter()
            plain.formatOptions = [.withInternetDateTime]
            if let d = plain.date(from: s) { return d }
            throw DecodingError.dataCorruptedError(
                in: try decoder.singleValueContainer(),
                debugDescription: "unrecognized date \(s)"
            )
        }
        return try? dec.decode(Snapshot.self, from: data)
    }

    static func menubarEnabled() -> Bool {
        guard let json = run(["menubar", "status"]),
              let data = json.data(using: .utf8),
              let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return false }
        return obj["enabled"] as? Bool ?? false
    }
}

// MARK: - Formatting

enum Fmt {
    static func hm(_ date: Date) -> String {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f.string(from: date)
    }

    static func duration(_ secs: Int) -> String {
        let m = max(0, secs) / 60
        return String(format: "%dh%02dm", m / 60, m % 60)
    }

    /// Time alone for today, prefixed with the weekday otherwise — the list can span
    /// days now, and a bare "05:30" would be ambiguous.
    static func stamp(_ date: Date) -> String {
        let f = DateFormatter()
        f.dateFormat = Calendar.current.isDateInToday(date) ? "HH:mm" : "EEE HH:mm"
        return f.string(from: date)
    }

    static func dayAndTime(_ date: Date) -> String {
        let f = DateFormatter()
        f.dateFormat = "EEE HH:mm"
        return f.string(from: date)
    }
}

// MARK: - Global hotkey

/// Posted from the Carbon callback, which must be a bare C function and so cannot
/// capture the delegate.
private let hotKeyFired = Notification.Name("ClaudePrimerHotKeyFired")

private func hotKeyCallback(_: EventHandlerCallRef?, _: EventRef?, _: UnsafeMutableRawPointer?) -> OSStatus {
    NotificationCenter.default.post(name: hotKeyFired, object: nil)
    return noErr
}

// MARK: - App

final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuDelegate {
    private var statusItem: NSStatusItem!
    private var timer: Timer?
    private var watcher: DispatchSourceFileSystemObject?
    private var watchedFD: CInt = -1
    private var latest: Snapshot?
    /// Last known real usage, kept so the menu can draw instantly instead of waiting on
    /// a subprocess. Refreshed on a slow timer and again (asynchronously) on open.
    private var cachedUsage: UsageInfo?
    /// The two rows the async fetch updates in place, so a late arrival doesn't have to
    /// rebuild a menu the user is already reading.
    private weak var sessionRow: NSMenuItem?
    private weak var weekRow: NSMenuItem?
    private var hotKeyRef: EventHotKeyRef?

    func applicationDidFinishLaunching(_: Notification) {
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        let menu = NSMenu()
        menu.delegate = self
        statusItem.menu = menu

        refresh()

        // The timer is the floor. The file watcher below is what makes a landed prime
        // show up immediately rather than up to 30s later.
        timer = Timer.scheduledTimer(withTimeInterval: 30, repeats: true) { [weak self] _ in
            self?.refresh()
        }
        startWatchingRunsLog()

        registerHotKey()
    }

    /// ⌃⌥C opens the menu from anywhere.
    ///
    /// Registration fails if another app already owns the combination; that is reported
    /// rather than swallowed, since a silently dead shortcut is indistinguishable from a
    /// broken app.
    private func registerHotKey() {
        NotificationCenter.default.addObserver(
            forName: hotKeyFired, object: nil, queue: .main
        ) { [weak self] _ in
            self?.statusItem.button?.performClick(nil)
        }

        var spec = EventTypeSpec(eventClass: OSType(kEventClassKeyboard),
                                 eventKind: UInt32(kEventHotKeyPressed))
        InstallEventHandler(GetApplicationEventTarget(), hotKeyCallback, 1, &spec, nil, nil)

        // 'CPrm', an arbitrary but unique signature for this app's hotkeys.
        let id = EventHotKeyID(signature: OSType(0x4350726D), id: 1)
        let status = RegisterEventHotKey(UInt32(kVK_ANSI_C),
                                         UInt32(controlKey | optionKey),
                                         id,
                                         GetApplicationEventTarget(),
                                         0,
                                         &hotKeyRef)
        if status != noErr {
            NSLog("claude-primer: could not register ⌃⌥C (error \(status)) — another app may own it")
        }
    }

    /// How long a usage reading stays good enough to reuse.
    ///
    /// Every fetch runs `claude`, which authenticates against the login keychain — the
    /// same credential the Claude Code VS Code extension holds. Doing that on a timer
    /// meant ~288 authentications a day and appeared to invalidate the extension's
    /// session, forcing repeated re-verification. It is now fetched only when the menu
    /// is opened, and not even then if a recent reading is still on hand.
    private static let usageMaxAge: TimeInterval = 120

    private var usageFetchedAt: Date?

    /// Fetch real usage off the main thread and cache it.
    private func refreshUsage() {
        if let at = usageFetchedAt, Date().timeIntervalSince(at) < Self.usageMaxAge { return }
        DispatchQueue.global(qos: .utility).async { [weak self] in
            let u = CLI.snapshot(withUsage: true)?.usage
            DispatchQueue.main.async {
                guard let self, let u else { return }
                self.cachedUsage = u
                self.usageFetchedAt = Date()
                self.applyUsage(u)
            }
        }
    }

    /// Update the two usage rows in place. Safe while the menu is open.
    private func applyUsage(_ u: UsageInfo) {
        if let pct = u.sessionPct, let r = sessionRow {
            let resets = u.sessionResetsAt.map { "resets \(Fmt.hm($0))" } ?? ""
            r.attributedTitle = row("Session", "\(pct)%", resets).attributedTitle
        }
        if let pct = u.weekPct, let r = weekRow {
            let resets = u.weekResetsAt.map { "resets \(Fmt.dayAndTime($0))" } ?? ""
            r.attributedTitle = row("This week", "\(pct)%", resets).attributedTitle
        }
    }

    func applicationWillTerminate(_: Notification) {
        if let hotKeyRef { UnregisterEventHotKey(hotKeyRef) }
        watcher?.cancel()
        if watchedFD >= 0 { close(watchedFD) }
    }

    // MARK: Refresh

    private func refresh() {
        // Off the main thread: this spawns a subprocess, and blocking the main thread
        // would freeze the menu bar for every other app too.
        DispatchQueue.global(qos: .utility).async { [weak self] in
            let snap = CLI.snapshot()
            DispatchQueue.main.async {
                self?.latest = snap
                self?.render(snap)
            }
        }
    }

    private func render(_ snap: Snapshot?) {
        guard let button = statusItem.button, let menu = statusItem.menu else { return }

        guard let snap else {
            button.image = Icon.unavailable
            button.title = " ?"
            fillUnavailable(menu)
            return
        }

        // Icon only. The countdown lives in the dropdown rather than the menu bar, so
        // the item stays narrow and doesn't visually tick. Health still shows here,
        // because a problem you have to open a menu to notice is a problem you miss.
        button.image = Icon.claude
        button.title = snap.health.prefix
        fill(menu, snap)
    }

    // MARK: Opening

    /// Fetch real usage only here. It costs no tokens but does spawn a subprocess and
    /// take ~0.5s, so paying for it on the 30s poll would be 2,880 calls a day to show
    /// a number nobody is looking at.
    /// Draw immediately from cached state, then refresh in the background. Fetching
    /// synchronously here blocked the menu from appearing for about a second, which is
    /// a poor trade for a percentage that barely moves.
    func menuWillOpen(_ menu: NSMenu) {
        if let snap = CLI.snapshot() {
            latest = snap
            fill(menu, snap)
        }
        refreshUsage()
    }

    // MARK: Menus

    private func fillUnavailable(_ m: NSMenu) {
        m.removeAllItems()
        m.addItem(disabled("claude-primer not responding"))
        m.addItem(disabled("expected at \(CLI.binary)"))
        m.addItem(.separator())
        m.addItem(withTitle: "Retry now", action: #selector(retry), keyEquivalent: "")
            .target = self
        m.addItem(.separator())
        addQuit(to: m)
    }

    /// Repopulates the *existing* menu rather than building a new one. Assigning a new
    /// NSMenu to the status item drops its delegate, which silently killed
    /// `menuWillOpen` — and with it the on-open usage fetch — on the first refresh.
    private func fill(_ m: NSMenu, _ snap: Snapshot) {
        m.removeAllItems()

        // Rows always exist so a late fetch can fill them without rebuilding the menu.
        let session = row("Session", "…", "")
        let week = row("This week", "…", "")
        m.addItem(session)
        m.addItem(week)
        m.addItem(.separator())
        sessionRow = session
        weekRow = week
        if let u = snap.usage ?? cachedUsage { applyUsage(u) }

        // The most recent prime, with its own outcome marker. This replaced the
        // estimated-window row: that estimate is inferred from this tool's own primes
        // and is simply wrong whenever a window was opened elsewhere, so showing it
        // beneath the real numbers above invited comparing a fact with a guess.
        if let last = snap.recent.last {
            m.addItem(row("Last prime", Fmt.dayAndTime(last.ts), last.marker))
        } else {
            m.addItem(row("Last prime", "none yet", "—"))
        }

        if let next = snap.nextPrime {
            m.addItem(row("Next prime", Fmt.dayAndTime(next.at), ""))
        }

        if snap.today.scheduled {
            let mark = snap.health == .ok ? "✓" : (snap.health == .warn ? "⚠" : "✗")
            m.addItem(row("Today", "\(snap.today.done)/\(snap.today.expected)", mark))
        } else {
            m.addItem(row("Today", "not scheduled", "—"))
        }

        if !snap.units.agent.loaded {
            m.addItem(disabled("⚠︎ agent not loaded — run: claude-primer install"))
        }

        m.addItem(.separator())
        add(to: m, "Prime now…", #selector(primeNow))
        add(to: m, "Open status in Terminal", #selector(openStatus))
        add(to: m, "Edit config…", #selector(editConfig))
        add(to: m, "Reveal logs in Finder", #selector(revealLogs))

        m.addItem(.separator())
        let launch = NSMenuItem(title: "Launch at login",
                                action: #selector(toggleLaunchAtLogin),
                                keyEquivalent: "")
        launch.target = self
        launch.state = CLI.menubarEnabled() ? .on : .off
        m.addItem(launch)

        addQuit(to: m)
    }

    /// A three-column row.
    ///
    /// Menus render in a proportional font, so padding with spaces does not line
    /// anything up. Real tab stops do, and `monospacedDigitSystemFont` keeps the digits
    /// themselves the same width — so 51% and 68%, or 17:29 and 05:59, sit in true
    /// columns while the labels stay in the system face rather than looking like a
    /// terminal.
    private func row(_ label: String, _ mid: String, _ right: String) -> NSMenuItem {
        let style = NSMutableParagraphStyle()
        style.tabStops = [
            NSTextTab(textAlignment: .left, location: 92),
            NSTextTab(textAlignment: .left, location: 168),
        ]
        let item = NSMenuItem(title: "", action: nil, keyEquivalent: "")
        item.attributedTitle = NSAttributedString(
            string: "\(label)\t\(mid)\t\(right)",
            attributes: [
                .font: NSFont.monospacedDigitSystemFont(ofSize: NSFont.systemFontSize, weight: .regular),
                .paragraphStyle: style,
            ]
        )
        item.isEnabled = false
        return item
    }

    private func disabled(_ title: String) -> NSMenuItem {
        let i = NSMenuItem(title: title, action: nil, keyEquivalent: "")
        i.isEnabled = false
        return i
    }

    private func add(to menu: NSMenu, _ title: String, _ action: Selector) {
        let i = NSMenuItem(title: title, action: action, keyEquivalent: "")
        i.target = self
        menu.addItem(i)
    }

    private func addQuit(to menu: NSMenu) {
        let q = NSMenuItem(title: "Quit", action: #selector(quit), keyEquivalent: "q")
        q.target = self
        menu.addItem(q)
    }

    // MARK: Actions

    @objc private func retry() { refresh() }

    /// The only action here that spends anything, so it asks first. A mis-click would
    /// open a 5-hour window at the wrong time and shift every later boundary — exactly
    /// the failure the scheduling exists to prevent.
    @objc private func primeNow() {
        let a = NSAlert()
        a.messageText = "Send a priming prompt now?"
        a.informativeText = """
            This spends a small amount of your usage quota and starts a new 5-hour \
            window beginning now, which will shift your remaining schedule for today.
            """
        a.alertStyle = .warning
        a.addButton(withTitle: "Prime now")
        a.addButton(withTitle: "Cancel")
        NSApp.activate(ignoringOtherApps: true)
        guard a.runModal() == .alertFirstButtonReturn else { return }

        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            CLI.run(["run", "--anchor", "manual", "--force"])
            DispatchQueue.main.async { self?.refresh() }
        }
    }

    @objc private func openStatus() {
        let script = """
            tell application "Terminal"
                activate
                do script "\(CLI.binary) status"
            end tell
            """
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/usr/bin/osascript")
        p.arguments = ["-e", script]
        try? p.run()
    }

    @objc private func editConfig() {
        guard let path = latest?.paths.config else { return }
        // NSWorkspace.open honours whatever the user has set as the .toml handler.
        NSWorkspace.shared.open(URL(fileURLWithPath: path))
    }

    @objc private func revealLogs() {
        guard let path = latest?.paths.runsLog else { return }
        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
    }

    @objc private func toggleLaunchAtLogin() {
        let enabled = CLI.menubarEnabled()
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            CLI.run(["menubar", enabled ? "disable" : "enable"])
            DispatchQueue.main.async { self?.refresh() }
        }
    }

    @objc private func quit() { NSApp.terminate(nil) }

    // MARK: Watching runs.jsonl

    /// Re-render as soon as a prime lands, instead of waiting out the 30s timer.
    /// The log is appended to, so `.write` is the event that matters; `.delete`/`.rename`
    /// mean the file was rotated or removed, so the watch is rebuilt against the new one.
    private func startWatchingRunsLog() {
        guard let path = latest?.paths.runsLog else {
            // No snapshot yet — try again once the first refresh has landed.
            DispatchQueue.main.asyncAfter(deadline: .now() + 2) { [weak self] in
                self?.startWatchingRunsLog()
            }
            return
        }

        watcher?.cancel()
        if watchedFD >= 0 { close(watchedFD) }

        watchedFD = open(path, O_EVTONLY)
        guard watchedFD >= 0 else { return }

        let src = DispatchSource.makeFileSystemObjectSource(
            fileDescriptor: watchedFD,
            eventMask: [.write, .delete, .rename],
            queue: .main
        )
        src.setEventHandler { [weak self] in
            guard let self else { return }
            let flags = src.data
            self.refresh()
            if flags.contains(.delete) || flags.contains(.rename) {
                // Rebuild the watch against whatever replaced the file.
                DispatchQueue.main.asyncAfter(deadline: .now() + 1) {
                    self.startWatchingRunsLog()
                }
            }
        }
        src.setCancelHandler { [weak self] in
            if let fd = self?.watchedFD, fd >= 0 { close(fd) }
            self?.watchedFD = -1
        }
        src.resume()
        watcher = src
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
// Accessory, not regular: no Dock icon and no menu bar takeover. Info.plist sets
// LSUIElement too; this makes it explicit even if the bundle is bypassed.
app.setActivationPolicy(.accessory)
app.run()
