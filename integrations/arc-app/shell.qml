// Arc — standalone desktop app (Quickshell).
//
// Three panes, all live, no polling:
//   TOOLS    every tool Arc can call, with its description and risk level, and
//            a per-tool safety selector (safe | caution | dangerous) that the
//            user drives directly. Setting it shells out to `arc tool <name>
//            <level>`; the daemon persists it, so it is still there after a
//            restart. Nothing here edits a config file.
//            (`arc --json tools`, loaded once, refreshable)
//   CHAT     what you said and what Arc answered
//   TRACE    the thought process of the selected turn: the model's reasoning,
//            each tool it called with its arguments, the result, any held
//            confirmation, and the reply
//
// Everything comes from `arc --json watch`, the same event stream the bar
// widget uses. Typing a request runs `arc ask`; its events arrive through the
// watch stream like a spoken request would, so voice and typed turns look the
// same here.
//
// Launch: `qs -c arc` (installed to ~/.config/quickshell/arc by install.sh).
import QtQuick
import QtQuick.Layouts
import QtQuick.Controls
import Quickshell
import Quickshell.Io

ShellRoot {
  id: shell

  readonly property string arcCmd: "arc"

  // ------------------------------------------------------------ palette
  //
  // Read from the live Omarchy theme rather than hardcoded, so the app is
  // the same colour as the bar, the launcher and the terminal, and it follows
  // `omarchy theme set` with no restart. Omarchy's own shell reads the same
  // file through the same Quickshell FileView.
  QtObject {
    id: c
    readonly property string home: Quickshell.env("HOME")
    readonly property string themePath: home + "/.local/state/omarchy/current/theme/colors.toml"

    // Raw "key" -> "#rrggbb" from the theme, or "" when the key is absent.
    // Assigning the whole object is what makes the bindings below re-evaluate
    // on a theme swap; mutating it in place would not.
    property var theme: ({})

    // Returns a *colour*, not the hex string. The legibility helpers below
    // read .r/.g/.b, and handing them a string made lum() NaN: the binary
    // search then chose black as the "away" colour on a light theme, blend()
    // returned Qt.rgba(NaN...), and every Text item was handed an invalid
    // colour and rendered nothing at all. The window came up blank.
    // A QML colour arrives as an object with r/g/b; a hex literal is a string.
    // Nested lookups pass a colour back in as the fallback, so accept both
    // rather than wrapping a colour in Qt.color() a second time.
    function asColor(x) {
      return (x !== null && typeof x === "object" && "r" in x) ? x : Qt.color(String(x))
    }
    function val(key, fallback) {
      var v = theme[key]
      return asColor(typeof v === "string" && v.length > 0 ? v : fallback)
    }
    // Mix toward another colour: used for the surface ramp, since a theme
    // gives one background and this UI needs three related ones.
    function blend(a, b, t) {
      return Qt.rgba(a.r * (1 - t) + b.r * t, a.g * (1 - t) + b.g * t, a.b * (1 - t) + b.b * t, 1)
    }

    // --- legibility ------------------------------------------------------
    //
    // A theme's palette is not a promise that its own colours work in this
    // app. Measured across the 27 themes Omarchy ships or has installed,
    // 20 had secondary text below 4.5:1 on these surfaces and 5 had an accent
    // below it -- rose-pine's muted was 1.55:1 and matte-black's 2.40:1, which
    // is the "black text on a dark background" this guards against. Themes
    // pick muted against their own idea of where text sits; this app stacks
    // three surfaces and puts type at 10px on them.
    //
    // So the theme chooses the colour and this decides whether it can be
    // read: if it clears the floor it is used untouched, and if it does not,
    // it is walked away from the background until it does. The hue survives
    // as far as it can -- only as much desaturation as the floor demands.
    // NOTE: a QML colour's r/g/b are already 0..1 floats. Dividing by 255 here
    // -- the reflex from every other language -- made every luminance ~0, so
    // contrast() was always 1, nothing was ever "readable", and readable()
    // handed back its away colour: white. White on a dark theme is
    // accidentally fine, which is why only the light themes broke.
    function lum(c) {
      var f = function (v) {
        return v <= 0.04045 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4)
      }
      return 0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
    }
    function contrast(a, b) {
      var la = lum(a), lb = lum(b)
      var hi = Math.max(la, lb), lo = Math.min(la, lb)
      return (hi + 0.05) / (lo + 0.05)
    }
    // Smallest shift away from `bg` that reaches `min` contrast, or the
    // colour unchanged if it already gets there. Binary search rather than a
    // fixed step, so a colour one shade short is barely touched.
    function readable(fg, bg, min) {
      if (contrast(fg, bg) >= min) return fg
      var away = lum(bg) < 0.5 ? Qt.rgba(1, 1, 1, 1) : Qt.rgba(0, 0, 0, 1)
      var lo = 0, hi = 1, best = away
      for (var i = 0; i < 20; i++) {
        var mid = (lo + hi) / 2
        var cand = blend(fg, away, mid)
        if (contrast(cand, bg) >= min) { best = cand; hi = mid } else { lo = mid }
      }
      return best
    }
    // Text lands on both the window and the panels, and which of the two is
    // the harder case flips with light and dark themes, so check both.
    function onBoth(fg, a, b, min) {
      return readable(readable(fg, a, min), b, min)
    }

    // One flat "#rrggbb" -> "#rrggbb" walker. Themes vary in which keys they
    // define, so every slot below has a chain of fallbacks ending at a
    // neutral that still reads correctly.
    function parseColors(raw) {
      var out = {}
      var re = /^\s*([A-Za-z0-9_-]+)\s*=\s*["']?(#[0-9A-Fa-f]{6})/
      var lines = String(raw || "").split("\n")
      for (var i = 0; i < lines.length; i++) {
        var m = lines[i].match(re)
        if (m) out[m[1]] = m[2]
      }
      return out
    }

    // --- surfaces, from darkest to lightest
    readonly property color bg: val("darker_background", val("dark_background", val("background", "#141414")))
    readonly property color panel: val("background", "#1c1c1c")
    readonly property color panelHi: val("lighter_background", blend(panel, text, 0.06))
    // Borders only need to be seen, not read, so the floor is low; 1.44:1 on
    // everforest and rose-pine was not seen.
    readonly property color line: onBoth(blend(text, bg, 0.72), panel, bg, 1.6)

    // --- text
    readonly property color text: onBoth(val("foreground", "#e6e6e6"), panel, bg, 4.5)
    // Nudged toward the text colour first, then floored. The nudge is what
    // makes it read as secondary rather than as a failed attempt at body
    // text; the floor is what makes it readable at all, which the nudge
    // cannot promise because a theme's muted may be nowhere near its
    // foreground.
    readonly property color muted: onBoth(blend(val("muted", val("dark_foreground", text)), text, 0.22), panel, bg, 4.5)

    // --- roles
    //
    // Omarchy's own shell paints its highlights with `accent` 235 times and
    // the numbered palette 9 times combined, so `accent` is the chrome colour
    // here too: the wordmark, every heading, the panel brackets, the timeline
    // and the send button. Under Itachi that is the crimson #dc5b56, which is
    // what the bar and launcher use.
    readonly property color accent: val("accent", val("color4", "#c05050"))
    readonly property color chrome: onBoth(accent, panel, bg, 4.5)
    // Still visibly secondary, but the status chips are text: 2.08:1 on
    // miasma, nord and white was not readable.
    readonly property color chromeDim: onBoth(blend(chrome, bg, 0.45), panel, bg, 3.0)

    // The safety colours mean safe, caution and danger, so they keep their
    // identity and only get pushed until they can be read -- which on a light
    // theme is most of them: rose-pine's caution amber was 1.63:1 and
    // catppuccin-latte's safe green 2.35:1.
    readonly property color magenta: onBoth(val("magenta", val("bright_magenta", val("color5", accent))), panel, bg, 4.5)
    readonly property color amber: onBoth(val("yellow", val("bright_yellow", val("color3", "#d0a040"))), panel, bg, 4.5)
    readonly property color green: onBoth(val("green", val("bright_green", val("color2", "#70a060"))), panel, bg, 4.5)
    readonly property color red: onBoth(val("red", val("bright_red", val("color1", "#c05050"))), panel, bg, 4.5)
    readonly property string mono: "JetBrainsMono Nerd Font"

    // True when two parsed themes are identical, so a poll that finds no
    // change does not reassign `theme` and repaint the whole app.
    function sameTheme(a, b) {
      var ka = Object.keys(a), kb = Object.keys(b)
      if (ka.length !== kb.length) return false
      for (var i = 0; i < ka.length; i++) if (a[ka[i]] !== b[ka[i]]) return false
      return true
    }
    function acceptColors(raw) {
      var p = parseColors(raw)
      if (Object.keys(p).length === 0) return   // a torn read; keep what we have
      if (Object.keys(p).length === 0) return   // a torn read; keep what we have
      if (!sameTheme(theme, p)) theme = p
    }

    property FileView colorsFile: FileView {
      id: colorsFile
      path: c.themePath
      watchChanges: true
      printErrors: false
      // reload() rather than parsing the change signal: text() is stale there.
      onLoaded: c.acceptColors(text())
      onFileChanged: reload()
      // Deliberately keeps the last good palette. `omarchy theme set` swaps
      // the whole current/theme directory, so the watched file briefly stops
      // existing; blanking the palette here left the app on its grey fallback
      // for good, because the watcher dies with the old inode.
      onLoadFailed: colorsPoll.restart()
    }
  }

  // At the root, not inside the palette object: a QtObject has no default
  // property, so a bare Timer there is a load error.
  Timer {
    id: colorsPoll
    interval: 3000
    repeat: true
    running: true
    onTriggered: colorsFile.reload()
  }

  function riskColor(r) {
    return r === "dangerous" ? c.red : r === "caution" ? c.amber : c.green
  }
  readonly property var levels: ["safe", "caution", "dangerous"]

  // ------------------------------------------------------------ state
  property string arcState: "offline"
  property string model: ""
  property var tools: []
  // Set while a classification is being written, so the row can show it and
  // ignore further clicks until the daemon answers.
  property string classBusy: ""
  // Last classification change, shown in the header. Empty until the user
  // touches one.
  property string classNote: ""
  // turns: [{id, query, source, at, steps:[...], reply, done}]
  // step:  {kind: thought|tool|confirm|error, ...}
  property var turns: []
  property int selected: -1          // index into turns; -1 = follow latest
  property int rev: 0                // bump to refresh bindings on mutation
  property string toolFilter: ""
  // The music section's state, exactly as the daemon reports it: the current
  // track, what is queued behind it, and whether Arc is the one playing it.
  // Null when nothing is playing -- the section then collapses to a search box
  // rather than disappearing, because an empty queue is where you add to it.
  property var music: null
  // Playhead, in seconds. Polled rather than evented: it is the only part of
  // the status that changes on its own every second, and an event per second
  // would repaint the whole app for a progress bar that can tick locally.
  property real position: 0
  property real duration: 0
  // True once the section has been expanded by the user. The position poll
  // only runs while it is open -- a widget nobody is looking at should not
  // spawn a process every second.
  property bool musicOpen: false
  // Which page is showing: "chat" (tools, conversation, thought process) or
  // "media" (the player and everything you can do to it). Media used to be a
  // collapsible strip above the panes, which put a queue you can add to and
  // remove from inside the same column as a chat transcript -- and made both
  // narrower than either wanted to be.
  property string tab: "chat"
  readonly property bool mediaTab: tab === "media"
  readonly property var tabs: [
    { id: "chat", label: "CHAT & TOOLS" },
    { id: "media", label: "MEDIA" }
  ]
  // A command that is in flight, so the buttons can ignore a second click
  // rather than queueing two skips.
  property string musicBusy: ""
  // The daemon's refusal of the last action, shown next to the buttons.
  property string musicNote: ""

  // `music.now` reached through one accessor. The panes read it in bindings
  // that also run while the section is collapsed and nothing is playing, and
  // `music.now.state` on a null music is a TypeError that blanks the window
  // rather than failing one row.
  readonly property var musicNow: (music && music.now) ? music.now : { state: "stopped", title: "", artist: "" }
  // Same for the queue. Reading `music.queue` in a binding that also runs
  // while nothing is playing throws "Cannot read property 'queue' of null" --
  // and a binding that throws is a binding the engine keeps re-evaluating, so
  // it spams the log once a second rather than failing once.
  readonly property var musicQueue: (music && music.queue) ? music.queue : []

  readonly property int shownIndex: selected >= 0 && selected < turns.length ? selected : turns.length - 1
  readonly property var shownTurn: { rev; return shownIndex >= 0 ? turns[shownIndex] : null }

  function now() { return Qt.formatTime(new Date(), "hh:mm:ss") }
  // Turns are mutated in place while events arrive, but QML only re-evaluates
  // a binding when the value it read changes identity. Publishing a fresh copy
  // of the live turn (and its steps) on every change is what makes a tool go
  // from "running" to "success" on screen.
  function touch() {
    if (turns.length) {
      var t = turns[turns.length - 1]
      var n = Object.assign({}, t)
      n.steps = t.steps.map(function(s) { return Object.assign({}, s) })
      var next = turns.slice(0, -1); next.push(n)
      turns = next
    }
    rev++
  }
  function current() { return turns.length ? turns[turns.length - 1] : null }

  function ensureTurn(query, source) {
    var t = { id: turns.length, query: query, source: source || "", at: now(), steps: [], reply: "", done: false, started: Date.now(), ms: 0 }
    var next = turns.slice(); next.push(t)
    if (next.length > 60) next.shift()
    turns = next
    return t
  }

  function onEvent(v) {
    var e = v.event
    if (e === "state") { arcState = String(v.state || "idle"); return }
    if (e === "heard") { ensureTurn(String(v.text || ""), String(v.source || "")); touch(); return }
    // Not a turn event: the music strip is independent of any request, so it
    // is handled before the "which turn does this belong to" logic below --
    // a track starting playback must not conjure an empty turn.
    if (e === "music") {
      // The daemon sends the whole status, including whether anything is
      // playing, so this never re-derives it -- a stopped status with a stale
      // title still clears the section.
      applyMusic(v.status || {})
      return
    }
    var t = current()
    if (!t || t.done) {
      if (e === "reply" || e === "thought" || e === "tool_started") t = ensureTurn("(no request text)", "")
      else if (e !== "error") return
    }
    if (e === "thought") {
      t.steps.push({ kind: "thought", round: v.round, reasoning: String(v.reasoning || ""), text: String(v.text || ""), at: now() })
    } else if (e === "code_progress") {
      // What Hermes is doing inside a `code` task. Collected into one step
      // that grows, rather than a step per call: a real task made 63 of
      // them, and 63 rows would bury the turn.
      var live = null
      for (var i = t.steps.length - 1; i >= 0; i--)
        if (t.steps[i].kind === "hermes") { live = t.steps[i]; break }
      if (!live) {
        live = { kind: "hermes", items: [], steps: 0, elapsed_s: 0, done: false, at: now() }
        t.steps.push(live)
      }
      live.steps = v.step || (live.steps + 1)
      live.elapsed_s = v.elapsed_s || live.elapsed_s
      // Keep the tail only: this is a live tail, not a log.
      live.items.push(String(v.tool || "") + (v.detail ? "  " + String(v.detail) : ""))
      while (live.items.length > 40) live.items.shift()
    } else if (e === "tool_finished" && v.record && v.record.tool === "code") {
      for (var j = t.steps.length - 1; j >= 0; j--)
        if (t.steps[j].kind === "hermes") { t.steps[j].done = true; break }
    } else if (e === "tool_started") {
      t.steps.push({ kind: "tool", tool: v.tool, args: v.args, risk: v.risk, status: "running", summary: "", ms: 0, at: now(), t0: Date.now() })
    } else if (e === "tool_finished") {
      var r = v.record || {}
      var hit = null
      for (var i = t.steps.length - 1; i >= 0; i--)
        if (t.steps[i].kind === "tool" && t.steps[i].tool === r.tool && t.steps[i].status === "running") { hit = t.steps[i]; break }
      if (!hit) { hit = { kind: "tool", tool: r.tool, args: r.args, risk: r.risk, at: now() }; t.steps.push(hit) }
      hit.status = String(r.outcome || "success")
      hit.summary = String(r.summary || "")
      hit.warning = String(r.warning || "")
      hit.ms = r.duration_ms || 0
      // Arc changed its own toolset: show the new list without a restart.
      if ((r.tool === "tool_create" || r.tool === "tool_delete") && hit.status === "success") {
        toolLoader.running = false; toolLoader.running = true
      }
    } else if (e === "confirmation_required") {
      var p = v.pending || {}
      t.steps.push({ kind: "confirm", id: p.confirmation_id, text: String(p.explanation || ""), risk: p.risk, at: now(), resolved: false })
    } else if (e === "reply") {
      t.reply = String(v.text || ""); t.done = true; t.ms = Date.now() - t.started
    } else if (e === "error") {
      if (t) t.steps.push({ kind: "error", text: (v.component || "") + ": " + (v.message || ""), at: now() })
    }
    touch()
  }

  function ask(text) {
    var q = text.trim()
    if (!q.length) return
    selected = -1
    Quickshell.execDetached([arcCmd, "ask", q])
  }

  // Change one tool's safety classification.
  //
  // Optimistic on purpose: the write is a socket round trip plus a disk write,
  // and a dropdown that lags a click behind feels broken. The row is patched
  // in place immediately and reloaded from the daemon afterwards, so a
  // failure corrects itself rather than leaving a lie on screen.
  function setClass(name, level) {
    if (classBusy) return
    if (level !== "safe" && level !== "caution" && level !== "dangerous" && level !== "default") return
    classBusy = name
    classNote = name + " → " + level
    var next = tools.map(function(t) {
      if (t.name !== name) return t
      var u = Object.assign({}, t)
      u.risk = level === "default" ? t.default_risk : level
      u.reclassified = level !== "default" && level !== t.default_risk
      return u
    })
    tools = next
    classSetter.command = [arcCmd, "tool", name, level]
    classSetter.running = true
  }

  // Runs the change, then reloads the list from the daemon, which is the
  // authority on what was stored. A refusal (lowering a dangerous-by-nature
  // tool) is shown in the header rather than just snapping the row back.
  Process {
    id: classSetter
    stderr: StdioCollector { id: classErr }
    onExited: function(code) {
      if (code !== 0) {
        var msg = classErr.text.replace(/^Error:\s*/, "").trim()
        shell.classNote = "✗ " + (msg || ("arc tool exited " + code))
        noteClear.restart()
      } else {
        noteClear.restart()
      }
      toolLoader.running = false; toolLoader.running = true
      shell.classBusy = ""
    }
  }
  Timer { id: noteClear; interval: 6000; onTriggered: shell.classNote = "" }

  function decide(step, yes) {
    // `step` may be a stale copy; mark the live one.
    for (var i = 0; i < turns.length; i++)
      turns[i].steps.forEach(function(s) { if (s.kind === "confirm" && s.id === step.id) s.resolved = true })
    touch()
    Quickshell.execDetached([arcCmd, yes ? "confirm" : "reject", step.id])
  }

  // ------------------------------------------------------------ processes
  // Adopt a status from the daemon.
  function applyMusic(s) {
    var now = s.now || {}
    music = (now.playing === true) ? s : null
    // Position is not in the event: it is polled. Keeping the last known value
    // means the bar does not jump back to zero on every track change.
    duration = Number(s.duration || 0)
  }

  // Read the whole status at start and on every reconnect, so a track already
  // playing when the app opened shows up without waiting for the next change.
  Process {
    id: musicLoader
    command: [shell.arcCmd, "--json", "music", "show"]
    running: true
    stdout: StdioCollector {
      onStreamFinished: {
        try { shell.applyMusic(JSON.parse(text)) } catch (e) {}
      }
    }
  }

  // Poll the playhead, but only while the section is open and something is
  // playing. Otherwise there is nothing to animate and nothing to ask.
  Process {
    id: positionLoader
    command: [shell.arcCmd, "--json", "music", "position"]
    running: shell.musicOpen && shell.music !== null && shell.positionTick
    stdout: StdioCollector {
      onStreamFinished: {
        try {
          var p = JSON.parse(text)
          shell.position = Number(p.position || 0)
          if (p.duration) shell.duration = Number(p.duration)
        } catch (e) {}
      }
    }
  }
  // The tick is a separate property from `running` so the one-second Timer can
  // re-arm the poll by flipping it: a Process that is already running does not
  // restart when its command is unchanged.
  property bool positionTick: true
  Timer {
    id: positionPulse
    interval: 1000
    repeat: true
    running: shell.musicOpen && shell.music !== null
    onTriggered: { shell.positionTick = false; shell.positionTick = true }
  }

  // One transport action. `musicCmd` is set and re-armed so repeated clicks
  // restart the process rather than being ignored -- skipping three times has
  // to move three tracks.
  property var musicCmd: []
  Process {
    id: musicAction
    command: shell.musicCmd
    running: shell.musicCmd.length > 0
    stdout: StdioCollector {
      onStreamFinished: {
        try {
          var r = JSON.parse(text)
          if (r && r.now) shell.applyMusic(r)
        } catch (e) {}
      }
    }
    stderr: StdioCollector { id: musicErr }
    onExited: function(code) {
      if (code !== 0) {
        var m = String(musicErr.text).replace(/^Error:\s*/, "").trim()
        // A refusal is worth showing: "nothing is queued after this track" is
        // the difference between a button that is broken and one that was
        // pressed at a moment when it had nothing to do.
        shell.musicNote = m || ("arc music exited " + code)
        noteClear.restart()
      } else {
        shell.musicNote = ""
      }
      shell.musicCmd = []
      shell.musicBusy = ""
    }
  }

  function musicAction_(args) {
    if (musicBusy) return
    musicBusy = args[0]
    musicCmd = [arcCmd, "music"].concat(args)
  }

  function musicAdd(query) {
    var q = String(query || "").trim()
    if (!q.length) return
    musicOpen = true
    musicAction_(["enqueue", q])
  }

  function musicClearQueue() { musicAction_(["clear"]) }
  // Jump the playhead. `secs` is clamped here as well as in the daemon
  // because a click on a zero-width bar divides by zero, and NaN sent to a
  // socket is a seek the player has to reject.
  function musicSeek(secs) {
    var s = Number(secs)
    if (!isFinite(s) || s < 0) return
    musicAction_(["seek", String(Math.floor(s))])
  }

  // The cover art for the current track, or "" for none.
  //
  // An accessor rather than a direct read so every binding that shows art
  // gets "" instead of a TypeError when nothing is playing -- the same reason
  // musicNow and musicQueue exist.
  readonly property string musicArt: {
    var t = shell.musicNow
    return (t && t.artwork) ? String(t.artwork) : ""
  }
  function musicToggle() { musicAction_(["toggle"]) }
  function musicNext() { musicAction_(["next"]) }
  function musicPrev() { musicAction_(["previous"]) }
  function musicStop() { musicAction_(["stop"]) }
  function musicRemove(i) { musicAction_(["remove", String(i)]) }

  // Seconds as m:ss, for the progress bar. A negative or absent position
  // would render as "-1:-3" otherwise.
  function mmss(v) {
    var s = Math.max(0, Math.floor(Number(v) || 0))
    return Math.floor(s / 60) + ":" + ("0" + (s % 60)).slice(-2)
  }

  // How far through the track we are, 0..1. Clamped because a live position
  // can briefly exceed the duration while a new file is being opened, and a
  // bar that overflows its track is a bug that looks like a design choice.
  readonly property real musicProgress: duration > 0 ? Math.min(1, Math.max(0, position / duration)) : 0

  Process {
    id: watcher
    command: [shell.arcCmd, "--json", "watch", "--topics", "assistant,errors"]
    running: true
    stdout: SplitParser {
      onRead: function(line) {
        try { var v = JSON.parse(line) } catch (e) { return }
        if (v.kind === "event") shell.onEvent(v)
      }
    }
    onExited: { shell.arcState = "offline"; reconnect.start() }
  }
  Timer { id: reconnect; interval: 3000; onTriggered: { watcher.running = true; toolLoader.running = true; statusLoader.running = true; musicLoader.running = true } }

  Process {
    id: toolLoader
    command: [shell.arcCmd, "--json", "tools"]
    running: true
    stdout: StdioCollector {
      onStreamFinished: {
        try {
          var list = JSON.parse(text)
          list.sort(function(a, b) { return a.category === b.category ? a.name.localeCompare(b.name) : a.category.localeCompare(b.category) })
          shell.tools = list
        } catch (e) {}
      }
    }
  }
  Process {
    id: statusLoader
    command: [shell.arcCmd, "--json", "status"]
    running: true
    stdout: StdioCollector {
      onStreamFinished: {
        try {
          var s = JSON.parse(text)
          shell.model = (s.ai_provider || "") + "  ·  " + (s.ai_model || "")
          if (shell.arcState === "offline") shell.arcState = String(s.state || "idle")
        } catch (e) {}
      }
    }
  }

  // ------------------------------------------------------------ window
  FloatingWindow {
    id: win
    title: "Arc"
    implicitWidth: 1360
    implicitHeight: 840
    color: c.bg
    visible: true
    onClosed: Qt.quit()

    // Backdrop. Two attempts at this drew lines -- a flat 32px graph paper,
    // then a perspective floor -- and a grid is a grid however it is skewed.
    // So there are no lines at all now. The depth comes from light: a bloom
    // of the theme's accent from above, a soft band of it at the horizon with
    // no hard edge, the sky darkening toward the floor, and a vignette. Every
    // colour is mixed from the palette, so it re-tints with the theme.
    Canvas {
      id: backdrop
      anchors.fill: parent
      // Repaint on a palette change, not only on resize. The first version
      // only repainted for width/height, so after `omarchy theme set` it kept
      // drawing the previous theme's colour.
      Connections {
        target: c
        function onChromeChanged() { backdrop.requestPaint() }
        function onBgChanged() { backdrop.requestPaint() }
        function onPanelChanged() { backdrop.requestPaint() }
      }

      onPaint: {
        var g = getContext("2d")
        var w = width, h = height
        g.clearRect(0, 0, w, h)
        var accent = Qt.rgba(c.chrome.r, c.chrome.g, c.chrome.b, 1)
        var horizon = h * 0.46

        // 1. Light source: a wide bloom from above and slightly ahead, so the
        //    window has a direction to it instead of being evenly filled.
        var bloom = g.createRadialGradient(w * 0.5, -h * 0.1, 0, w * 0.5, -h * 0.1, h * 1.05)
        bloom.addColorStop(0, Qt.rgba(accent.r, accent.g, accent.b, 0.16))
        bloom.addColorStop(0.4, Qt.rgba(accent.r, accent.g, accent.b, 0.055))
        bloom.addColorStop(1, Qt.rgba(accent.r, accent.g, accent.b, 0))
        g.fillStyle = bloom
        g.fillRect(0, 0, w, h)

        // 2. The horizon as atmosphere, not as a rule: a wide band of accent
        //    that peaks at the horizon and falls off both ways. It reads as
        //    distance without drawing a single edge.
        var band = g.createLinearGradient(0, horizon - h * 0.30, 0, horizon + h * 0.34)
        band.addColorStop(0.00, Qt.rgba(accent.r, accent.g, accent.b, 0))
        band.addColorStop(0.46, Qt.rgba(accent.r, accent.g, accent.b, 0.085))
        band.addColorStop(0.50, Qt.rgba(accent.r, accent.g, accent.b, 0.11))
        band.addColorStop(0.54, Qt.rgba(accent.r, accent.g, accent.b, 0.085))
        band.addColorStop(1.00, Qt.rgba(accent.r, accent.g, accent.b, 0))
        g.fillStyle = band
        g.fillRect(0, horizon - h * 0.30, w, h * 0.64)

        // 3. Below the horizon the light falls off into the floor, so the lower
        //    half has weight instead of being flat.
        var floor = g.createLinearGradient(0, horizon, 0, h)
        floor.addColorStop(0, Qt.rgba(c.bg.r, c.bg.g, c.bg.b, 0))
        floor.addColorStop(1, Qt.rgba(c.bg.r, c.bg.g, c.bg.b, 0.55))
        g.fillStyle = floor
        g.fillRect(0, horizon, w, h - horizon)

        // 4. Vignette in the background's own colour rather than black, so it
        //    darkens a dark theme and does nothing ugly to a light one.
        var vig = g.createRadialGradient(
          w / 2, h / 2, Math.min(w, h) * 0.42,
          w / 2, h / 2, Math.max(w, h) * 0.86)
        vig.addColorStop(0, Qt.rgba(c.bg.r, c.bg.g, c.bg.b, 0))
        vig.addColorStop(1, Qt.rgba(c.bg.r, c.bg.g, c.bg.b, 0.40))
        g.fillStyle = vig
        g.fillRect(0, 0, w, h)
      }
    }

    ColumnLayout {
      anchors.fill: parent
      anchors.margins: 14
      spacing: 12

      // ---------------------------------------------------- header
      RowLayout {
        Layout.fillWidth: true
        spacing: 14
        Item {
          width: 34; height: 34
          Rectangle { id: core; anchors.centerIn: parent; width: 14; height: 14; radius: 7; color: shell.arcState === "offline" ? c.red : c.chrome }
          Rectangle {
            anchors.centerIn: parent; width: 30; height: 30; radius: 15; color: "transparent"
            border.color: core.color; border.width: 1.5; opacity: 0.8
            SequentialAnimation on scale {
              running: shell.arcState !== "idle" && shell.arcState !== "offline"; loops: Animation.Infinite
              NumberAnimation { from: 0.7; to: 1.15; duration: 700; easing.type: Easing.OutQuad }
              NumberAnimation { from: 1.15; to: 0.7; duration: 700; easing.type: Easing.InQuad }
            }
          }
        }
        Column {
          Text { text: "A R C"; color: c.chrome; font.family: c.mono; font.pixelSize: 22; font.bold: true; font.letterSpacing: 4 }
          Text { text: shell.model || "connecting…"; color: c.muted; font.family: c.mono; font.pixelSize: 11 }
        }
        Item { Layout.fillWidth: true }
        Tag { label: shell.arcState.toUpperCase(); tint: shell.arcState === "offline" ? c.red : shell.arcState === "idle" ? c.chromeDim : c.chrome }
        Tag { label: shell.tools.length + " TOOLS"; tint: c.chromeDim }
        // Echoes the last classification change, so a click is confirmed
        // somewhere other than in the row the user was looking at.
        Tag { visible: shell.classNote !== ""; label: shell.classNote; tint: c.amber }
        Tag { label: shell.turns.length + " TURNS"; tint: c.chromeDim }

        // The music section's compact form: just the track, so a glance at the
        // header still tells you what is playing. Everything else -- transport,
        // progress, the queue -- lives in the MUSIC pane, which this opens.
        Rectangle {
          id: musicChip
          visible: shell.music !== null
          Layout.preferredWidth: Math.min(chipRow.implicitWidth + 34, Math.max(200, win.width * 0.34))
          Layout.minimumWidth: 120
          height: 24
          radius: 3
          color: chipMa.containsMouse
            ? Qt.rgba(c.magenta.r, c.magenta.g, c.magenta.b, 0.24)
            : Qt.rgba(c.magenta.r, c.magenta.g, c.magenta.b, 0.14)
          border.color: c.magenta
          MouseArea {
            id: chipMa
            anchors.fill: parent
            hoverEnabled: true
            cursorShape: Qt.PointingHandCursor
            onClicked: { shell.tab = "media"; shell.musicOpen = true }
          }
          RowLayout {
            id: chipRow
            anchors.left: parent.left
            anchors.leftMargin: 10
            anchors.verticalCenter: parent.verticalCenter
            anchors.right: parent.right
            anchors.rightMargin: 10
            spacing: 6
            Text { text: "♫"; color: c.magenta; font.pixelSize: 12 }
            // Title and artist as separate items rather than the daemon's
            // combined label, so a long title elides and the artist survives.
            Text {
              text: String(shell.music ? shell.music.now.title : "")
              color: c.text
              font.family: c.mono; font.pixelSize: 11; font.bold: true
              elide: Text.ElideRight
              Layout.maximumWidth: 260
            }
            Text {
              text: String(shell.music ? (shell.music.now.artist || "") : "")
              visible: text.length > 0
              color: c.muted
              font.family: c.mono; font.pixelSize: 11
              elide: Text.ElideRight
              Layout.maximumWidth: 200
            }
            // A paused track says so here rather than looking identical to a
            // playing one -- the glyph alone is ambiguous at 11px.
            Text {
              visible: shell.music && shell.music.now.state === "paused"
              text: "PAUSED"
              color: c.amber
              font.family: c.mono; font.pixelSize: 9; font.bold: true
            }
            // How much is queued, which is the only number worth stealing
            // header space for.
            Text {
              visible: shell.musicQueue.length > 0
              text: "+" + shell.musicQueue.length
              color: c.magenta
              font.family: c.mono; font.pixelSize: 10
            }
          }
        }
      }
      Rectangle { Layout.fillWidth: true; height: 1; color: c.chrome; opacity: 0.35 }

      // ---------------------------------------------------- tabs
      //
      // Two pages, because the two things do not want the same shape. Chat
      // and tools are three narrow columns that need vertical room for text;
      // media is one wide page that needs horizontal room for cover art, a
      // queue and a transport. Sharing one column made both worse, so the
      // music section became a page of its own.
      RowLayout {
        Layout.fillWidth: true
        spacing: 4
        Repeater {
          model: shell.tabs
          delegate: Rectangle {
            id: tabBtn
            required property var modelData
            readonly property bool active: shell.tab === modelData.id
            // MEDIA carries a live badge: a track playing on the other tab is
            // worth knowing about without switching over to check.
            readonly property bool hasTrack: modelData.id === "media" && shell.music !== null
            width: tabLbl.implicitWidth + 32
            height: 28
            radius: 3
            color: active
              ? Qt.rgba(c.chrome.r, c.chrome.g, c.chrome.b, 0.16)
              : tabMa.containsMouse ? c.panelHi : "transparent"
            border.color: active ? c.chrome : c.line
            Text {
              id: tabLbl
              anchors.centerIn: parent
              text: (modelData.id === "media" && shell.musicNow.state !== "stopped" ? "\u266b " : "") + modelData.label
              color: tabBtn.active ? c.chrome : c.muted
              font.family: c.mono; font.pixelSize: 11; font.bold: true; font.letterSpacing: 2
            }
            // A playing dot, so MEDIA is not just another inactive tab.
            Rectangle {
              visible: tabBtn.hasTrack && !tabBtn.active
              width: 5; height: 5; radius: 3
              anchors.right: parent.right; anchors.rightMargin: 8
              anchors.verticalCenter: parent.verticalCenter
              color: c.magenta
              SequentialAnimation on opacity {
                running: tabBtn.hasTrack && !tabBtn.active
                loops: Animation.Infinite
                NumberAnimation { from: 1.0; to: 0.25; duration: 900; easing.type: Easing.InOutQuad }
                NumberAnimation { from: 0.25; to: 1.0; duration: 900; easing.type: Easing.InOutQuad }
              }
            }
            MouseArea {
              id: tabMa
              anchors.fill: parent
              hoverEnabled: true
              cursorShape: Qt.PointingHandCursor
              onClicked: { shell.tab = tabBtn.modelData.id; shell.musicOpen = tabBtn.modelData.id === "media" }
            }
          }
        }
        Item { Layout.fillWidth: true }
      }

      // ---------------------------------------------------- media page
      //
      // A page, not a section. This is where everything about the current
      // song lives: the art, the bubble, the playhead, every control, the
      // full queue, and the search that fills it. The chat page keeps only
      // the compact chip, so a glance at it still says what is playing.
      Item {
        id: musicPane
        Layout.fillWidth: true
        Layout.fillHeight: true
        visible: shell.mediaTab

        Rectangle {
          anchors.fill: parent
          radius: 6
          color: Qt.rgba(c.panel.r, c.panel.g, c.panel.b, 0.94)
          border.color: shell.music !== null ? c.magenta : c.line
        }

        RowLayout {
          id: musicBody
          anchors.left: parent.left
          anchors.right: parent.right
          anchors.top: parent.top
          anchors.bottom: parent.bottom
          anchors.margins: 14
          spacing: 18

          // --- the stage: cover art with the bubble moving over it
          //
          // Fixed width rather than proportional, because the art is a square
          // and a square in a fluid column is either letterboxed or cropped.
          // The bubble sits on top of it rather than beside it so the motion
          // reads as coming out of the record.
          ColumnLayout {
            Layout.preferredWidth: 232
            Layout.fillHeight: true
            spacing: 10

            Item {
              Layout.fillWidth: true
              Layout.fillHeight: true
              Layout.minimumHeight: 200

              Rectangle {
                id: artFrame
                anchors.centerIn: parent
                width: Math.min(parent.width, parent.height)
                height: width
                radius: 6
                color: Qt.rgba(c.text.r, c.text.g, c.text.b, 0.05)
                border.color: c.line
                clip: true

                // The cover the YouTube Music API returned. `asynchronous` so
                // a slow CDN cannot block the whole page's paint, and
                // `cache: false` because Quickshell's disk cache holds a
                // stale thumbnail across track changes otherwise.
                Image {
                  id: coverArt
                  anchors.fill: parent
                  asynchronous: true
                  cache: false
                  fillMode: Image.PreserveAspectCrop
                  source: shell.musicArt
                  opacity: status === Image.Ready ? 1 : 0
                  Behavior on opacity { NumberAnimation { duration: 260 } }
                }

                // Shown only while there is no art to show, so the square is
                // never an empty grey box with nothing in it.
                Text {
                  anchors.centerIn: parent
                  visible: !coverArt.visible || coverArt.status === Image.Error
                  text: "\u266b"
                  color: Qt.rgba(c.chromeDim.r, c.chromeDim.g, c.chromeDim.b, 0.5)
                  font.pixelSize: 46
                }
              }

              // The spectrum. Bar lengths are a function of the playhead, so
              // it reads the track rather than animating on its own clock.
              Visualiser_ {
                id: visualiser
                anchors.fill: parent
                visible: shell.music !== null
                progress: shell.musicProgress
                playing: shell.musicNow.state === "playing"
                paused: shell.musicNow.state === "paused"
              }
            }

            // The one-line identity under the art, so the art is never the
            // only way to know what is playing.
            ColumnLayout {
              Layout.fillWidth: true
              spacing: 2
              visible: shell.music !== null
              Text {
                Layout.fillWidth: true
                text: String(shell.music ? shell.music.now.title : "")
                color: c.text
                font.family: c.mono; font.pixelSize: 13; font.bold: true
                elide: Text.ElideRight
              }
              Text {
                Layout.fillWidth: true
                text: String(shell.music ? (shell.music.now.artist || "") : "")
                visible: text.length > 0
                color: c.muted
                font.family: c.mono; font.pixelSize: 11
                elide: Text.ElideRight
              }
            }
          }

          ColumnLayout {
            Layout.fillWidth: true
            Layout.fillHeight: true
            spacing: 10

            // --- the current track, or the field that starts one
            RowLayout {
              Layout.fillWidth: true
              spacing: 8
              visible: shell.music !== null
              Text {
                Layout.fillWidth: true
                text: String(shell.music ? shell.music.now.title : "")
                color: c.text
                font.family: c.mono; font.pixelSize: 12; font.bold: true
                elide: Text.ElideRight
              }
              Text {
                text: String(shell.music ? (shell.music.now.artist || "") : "")
                visible: text.length > 0
                color: c.muted
                font.family: c.mono; font.pixelSize: 11
                elide: Text.ElideRight
                Layout.maximumWidth: 220
              }
              Text {
                text: shell.musicNow.state === "paused" ? "PAUSED" : ""
                visible: shell.musicNow.state === "paused"
                color: c.amber
                font.family: c.mono; font.pixelSize: 9; font.bold: true
              }
            }

            // --- the current track, stated rather than illustrated
            RowLayout {
              Layout.fillWidth: true
              spacing: 8
              visible: shell.music !== null
              Text {
                text: "\u266b"
                color: c.magenta; font.pixelSize: 15
              }
              Text {
                Layout.fillWidth: true
                text: String(shell.music ? shell.music.now.title : "")
                color: c.text
                font.family: c.mono; font.pixelSize: 15; font.bold: true
                elide: Text.ElideRight
              }
              Text {
                text: String(shell.music ? (shell.music.now.artist || "") : "")
                visible: text.length > 0
                color: c.muted
                font.family: c.mono; font.pixelSize: 12
                elide: Text.ElideRight
                Layout.maximumWidth: 260
              }
              Text {
                text: shell.musicNow.state === "paused" ? "PAUSED" : ""
                visible: shell.musicNow.state === "paused"
                color: c.amber
                font.family: c.mono; font.pixelSize: 9; font.bold: true
              }
              Text {
                text: String(shell.music ? (shell.music.controllable ? "" : "REPORTED \u00b7 NOT ARC") : "")
                visible: shell.music !== null && !shell.music.controllable
                color: c.amber
                font.family: c.mono; font.pixelSize: 9; font.bold: true
              }
            }

            // --- progress. Hidden until the player knows how long the track
            // is: a bar that fills from 0 to 0 forever reads as broken.
            //
            // Clickable, because a progress bar you can only look at is the
            // one control every other player has and this one did not. The
            // hover handle is the only affordance: it appears where the
            // pointer is, so the gesture is discoverable without a label.
            RowLayout {
              Layout.fillWidth: true
              spacing: 10
              visible: shell.music !== null && shell.duration > 0
              Text {
                text: shell.mmss(shell.position)
                color: c.muted; font.family: c.mono; font.pixelSize: 10
              }
              Item {
                id: seekBar
                Layout.fillWidth: true
                height: 18
                // Click-to-seek only when Arc is the one playing: a track
                // Arc merely reported cannot be scrubbed, and offering the
                // gesture would be offering a button that cannot work.
                enabled: shell.music !== null && shell.music.controllable && shell.duration > 0
                // The cursor goes on the MouseArea, not here: a bare Item has
                // no `cursorShape` property, and assigning to a missing one is
                // a load-time warning that the seek bar then quietly loses.
                readonly property bool seekable: enabled
                readonly property real hoverX: seekMa.mouseX < 0 ? -1
                  : Math.max(0, Math.min(width, seekMa.mouseX))
                Rectangle {
                  anchors.verticalCenter: parent.verticalCenter
                  x: 0; width: parent.width; height: 5; radius: 2.5
                  color: Qt.rgba(c.text.r, c.text.g, c.text.b, 0.12)
                  Rectangle {
                    width: Math.max(0, seekBar.width * shell.musicProgress)
                    height: parent.height; radius: 2.5
                    color: shell.musicNow.state === "paused" ? c.amber : c.magenta
                  }
                  // Hover marker, plus a wider hit area than it looks.
                  Rectangle {
                    visible: seekBar.hoverX >= 0
                    x: seekBar.hoverX - width / 2
                    anchors.verticalCenter: parent.verticalCenter
                    width: 3; height: parent.height + 8; radius: 1.5
                    color: c.text
                  }
                }
                MouseArea {
                  id: seekMa
                  anchors.fill: parent
                  hoverEnabled: true
                  enabled: seekBar.seekable
                  cursorShape: seekBar.seekable ? Qt.PointingHandCursor : Qt.ArrowCursor
                  // The x of the press, not the current position: clicking
                  // seeks to where you clicked even if the poll has not run
                  // since the last tick.
                  onClicked: function(mouse) {
                    if (!seekBar.seekable || seekBar.width <= 0) return
                    var frac = Math.max(0, Math.min(1, mouse.x / seekBar.width))
                    shell.musicSeek(frac * shell.duration)
                  }
                }
              }
              Text {
                text: shell.mmss(shell.duration)
                color: c.muted; font.family: c.mono; font.pixelSize: 10
              }
            }

            // --- transport: every action Arc can take on the player.
            //
            // Pause and resume are one button rather than two, because the
            // state decides the glyph and a disabled-looking pair of separate
            // buttons for one action is a control that looks broken half the
            // time. Everything else is its own button, disabled with a
            // tooltip saying why rather than silently inert.
            RowLayout {
              Layout.fillWidth: true
              spacing: 6
              visible: shell.music !== null
              enabled: shell.music !== null && shell.music.controllable
              Transport_ {
                glyph: "\u23ee"; tip: "previous \u00b7 restart, or the one before"
                onClicked: shell.musicPrev()
              }
              Transport_ {
                glyph: shell.musicNow.state === "paused" ? "\u25b6" : "\u23f8"
                tip: shell.musicNow.state === "paused" ? "resume" : "pause"
                active: shell.musicNow.state === "paused"
                // Bigger than its neighbours: play/pause is the one control
                // pressed without looking.
                scale: 1.25
                onClicked: shell.musicToggle()
              }
              Transport_ {
                glyph: "\u23ed"
                tip: shell.musicQueue.length > 0
                  ? "next \u00b7 " + shell.musicQueue.length + " queued"
                  : "next \u00b7 nothing queued"
                enabled: shell.musicQueue.length > 0
                onClicked: shell.musicNext()
              }
              Transport_ { glyph: "\u2715"; tip: "stop and empty the queue"; tint: c.red; onClicked: shell.musicStop() }
              Rectangle { width: 1; height: 20; Layout.alignment: Qt.AlignVCenter; color: c.line }
              Button_ {
                label: "CLEAR QUEUE"
                tint: c.magenta
                // Disabled, not hidden: a queue that cannot be cleared is a
                // state the user should be able to see is a state.
                enabled: shell.musicQueue.length > 0
                opacity: enabled ? 1 : 0.35
                onClicked: shell.musicClearQueue()
              }
              Item { Layout.fillWidth: true }
              // The refusal, not a silent no-op. It has to be visible here:
              // these buttons do nothing when there is nothing queued, and a
              // button that silently does nothing is indistinguishable from a
              // broken one.
              Text {
                Layout.maximumWidth: Math.max(160, win.width * 0.3)
                visible: shell.musicNote !== ""
                text: shell.musicNote
                color: c.amber
                font.family: c.mono; font.pixelSize: 10
                elide: Text.ElideRight
              }
            }

            // --- what is coming
            ListView {
              id: queueList
              Layout.fillWidth: true
              // Fills whatever the page has left rather than a fixed five
              // rows: on the media tab the queue is the main event, and
              // capping it at five made a long queue need scrolling to see
              // the fifth thing.
              Layout.fillHeight: true
              Layout.minimumHeight: 60
              visible: count > 0
              clip: true
              spacing: 2
              model: shell.musicQueue
              ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }
              delegate: Rectangle {
                required property int index
                required property var modelData
                width: queueList.width - 6
                height: 28
                radius: 3
                color: rowMa.containsMouse ? c.panelHi : "transparent"
                MouseArea {
                  id: rowMa
                  anchors.fill: parent
                  hoverEnabled: true
                  onClicked: shell.musicRemove(index)
                }
                RowLayout {
                  anchors.fill: parent
                  anchors.leftMargin: 8
                  anchors.rightMargin: 6
                  spacing: 8
                  Text {
                    text: String(index + 1) + "."
                    color: c.chromeDim
                    font.family: c.mono; font.pixelSize: 10
                  }
                  Text {
                    Layout.fillWidth: true
                    text: String(modelData.title || "")
                    color: c.muted
                    font.family: c.mono; font.pixelSize: 11
                    elide: Text.ElideRight
                  }
                  Text {
                    Layout.maximumWidth: 180
                    text: String(modelData.artist || "")
                    visible: text.length > 0
                    color: c.chromeDim
                    font.family: c.mono; font.pixelSize: 10
                    elide: Text.ElideRight
                  }
                  // Remove is a click on the row, and says so on hover --
                  // clicking a row to delete something is not a safe default
                  // to leave unannounced.
                  Text {
                    text: "\u2715"
                    color: rowMa.containsMouse ? c.red : c.chromeDim
                    font.pixelSize: 10
                  }
                }
              }
            }
          }

          // --- search and actions
          //
          // Its own column because it is the only part of the page that is
          // about *starting* something rather than about what is playing.
          ColumnLayout {
            Layout.preferredWidth: Math.max(300, win.width * 0.26)
            Layout.fillHeight: true
            spacing: 8

            Field {
              id: musicQuery
              Layout.fillWidth: true
              placeholder: "search YouTube Music \u2014 artist, track, anything"
              onAccepted: function(t) { shell.musicAdd(t); clear() }
            }
            RowLayout {
              Layout.fillWidth: true
              spacing: 6
              // Play replaces the queue; add does not. Both are here because
              // they are different intentions and guessing is how you lose a
              // queue you cared about.
              Button_ {
                label: "PLAY NOW"
                tint: c.magenta
                onClicked: {
                  var q = musicQuery.value.trim()
                  if (!q.length) return
                  shell.musicAction_(["play", q])
                  musicQuery.clear()
                }
              }
              Button_ {
                label: "ADD TO QUEUE"
                onClicked: { shell.musicAdd(musicQuery.value); musicQuery.clear() }
              }
              Item { Layout.fillWidth: true }
            }

            Rectangle { Layout.fillWidth: true; height: 1; color: c.line }

            // Everything Arc can do to the player, in one list. Buttons above
            // are the fast path; this is the part you read when you want to
            // know what the controls do, and it is where a refusal from the
            // daemon is reported rather than silently swallowed.
            ColumnLayout {
              Layout.fillWidth: true
              spacing: 4
              Text {
                text: "ACTIONS"
                color: c.chrome; font.family: c.mono; font.pixelSize: 10
                font.bold: true; font.letterSpacing: 2
              }
              Repeater {
                model: [
                  { cmd: ["toggle"],      label: "Play / pause",        glyph: "\u23f8" },
                  { cmd: ["previous"],    label: "Previous track",      glyph: "\u23ee" },
                  { cmd: ["next"],        label: "Next track",          glyph: "\u23ed" },
                  { cmd: ["stop"],        label: "Stop and empty queue",glyph: "\u2715", danger: true },
                  { cmd: ["clear"],       label: "Clear upcoming only", glyph: "\u2327" },
                  { cmd: ["remove", "0"], label: "Remove next queued",  glyph: "\u2717" }
                ]
                delegate: Rectangle {
                  id: act
                  required property var modelData
                  readonly property bool usable: act.modelData.cmd[0] === "next" || act.modelData.cmd[0] === "clear" || act.modelData.cmd[0] === "remove"
                    ? shell.musicQueue.length > 0
                    : shell.music !== null
                  width: parent ? parent.width : 0
                  height: 26
                  radius: 3
                  color: actMa.containsMouse && act.usable ? c.panelHi : "transparent"
                  // Every action is listed even when it cannot run right now,
                  // greyed with its reason, because an action list that
                  // changes shape as the queue empties is one you cannot read.
                  opacity: act.usable ? 1 : 0.35
                  RowLayout {
                    anchors.fill: parent
                    anchors.leftMargin: 8; anchors.rightMargin: 8
                    spacing: 8
                    Text {
                      text: act.modelData.glyph
                      color: act.modelData.danger ? c.red : c.chrome
                      font.pixelSize: 11
                    }
                    Text {
                      Layout.fillWidth: true
                      text: act.modelData.label
                      color: c.text
                      font.family: c.mono; font.pixelSize: 11
                      elide: Text.ElideRight
                    }
                    Text {
                      visible: act.usable
                      text: "run"
                      color: c.chromeDim
                      font.family: c.mono; font.pixelSize: 9
                    }
                  }
                  MouseArea {
                    id: actMa
                    anchors.fill: parent
                    hoverEnabled: true
                    // Disabled rather than hidden, and the cursor says so:
                    // a row that greys out but still takes the click and
                    // refuses is a control that feels broken.
                    enabled: act.usable
                    cursorShape: act.usable ? Qt.PointingHandCursor : Qt.ArrowCursor
                    onClicked: shell.musicAction_(act.modelData.cmd)
                  }
                }
              }
            }

            Item { Layout.fillHeight: true }

            // The daemon's refusal. Shown here as well as by the transport:
            // "next" with nothing queued says so rather than doing nothing,
            // which is the difference between a button that is broken and one
            // that was pressed at a moment when it had no work.
            Rectangle {
              Layout.fillWidth: true
              visible: shell.musicNote !== ""
              implicitHeight: noteText.implicitHeight + 16
              radius: 3
              color: Qt.rgba(c.amber.r, c.amber.g, c.amber.b, 0.10)
              border.color: c.amber
              Text {
                id: noteText
                x: 8; y: 8; width: parent.width - 16
                text: shell.musicNote
                color: c.amber
                font.family: c.mono; font.pixelSize: 10
                wrapMode: Text.Wrap
              }
            }

            Text {
              Layout.fillWidth: true
              text: "queue \u00b7 " + shell.musicQueue.length + (shell.musicQueue.length > 0 ? " waiting" : "")
                + "\nresolved through the YouTube Music API"
              color: c.chromeDim
              font.family: c.mono; font.pixelSize: 10
              wrapMode: Text.Wrap
            }
          }
        }

      }

      // ---------------------------------------------------- panes
      //
      // Only on the chat page. The media page above owns the same slot, so
      // the two swap rather than stack -- the point of the tab is that the
      // player gets the whole window when you want the player.
      RowLayout {
        Layout.fillWidth: true
        Layout.fillHeight: true
        spacing: 12
        visible: !shell.mediaTab

        // ============ TOOLS
        Pane_ {
          Layout.preferredWidth: Math.max(250, win.width * 0.24)
          Layout.fillHeight: true
          title: "TOOLS"
          ColumnLayout {
            anchors.fill: parent
            spacing: 8
            Field {
              Layout.fillWidth: true
              placeholder: "filter tools…"
              onEdited: function(t) { shell.toolFilter = t.toLowerCase() }
            }
            ListView {
              id: toolList
              Layout.fillWidth: true
              Layout.fillHeight: true
              clip: true
              spacing: 6
              model: shell.tools.filter(function(t) {
                var f = shell.toolFilter
                return !f || t.name.indexOf(f) >= 0 || t.description.toLowerCase().indexOf(f) >= 0 || t.category.indexOf(f) >= 0
              })
              ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }
              delegate: Rectangle {
                required property var modelData
                property bool open: false
                width: toolList.width - 8
                height: tcol.implicitHeight + 16
                radius: 4
                color: ma.containsMouse ? c.panelHi : "transparent"
                border.color: open ? c.chromeDim : c.line
                opacity: modelData.enabled ? 1 : 0.45
                Rectangle { width: 3; height: parent.height - 12; anchors.verticalCenter: parent.verticalCenter; x: 0; radius: 1; color: shell.riskColor(modelData.risk) }
                // Declared before the content on purpose: a MouseArea takes
                // clicks from anything painted above it, so the row-expander
                // would otherwise eat every click on the safety picker.
                MouseArea { id: ma; anchors.fill: parent; hoverEnabled: true; onClicked: parent.open = !parent.open }
                Column {
                  id: tcol
                  x: 12; y: 8; width: parent.width - 20; spacing: 4
                  Row {
                    spacing: 8
                    Text { text: modelData.name; color: c.text; font.family: c.mono; font.pixelSize: 13; font.bold: true }
                    Text { text: modelData.made_by_arc ? "✦ " + modelData.category : modelData.category; color: modelData.made_by_arc ? c.magenta : c.muted; font.family: c.mono; font.pixelSize: 10; anchors.baseline: parent.children[0].baseline }
                  }
                  // The per-tool safety selector, on its own line so it can
                  // never be pushed past the row edge by a long tool name.
                  RiskPicker {
                    tool: modelData.name
                    current: modelData.risk
                    builtin: modelData.default_risk || modelData.risk
                    lowerable: modelData.lowerable !== false
                    reclassified: modelData.reclassified === true
                    enabled: shell.classBusy === ""
                    onPicked: function(level) { shell.setClass(modelData.name, level) }
                  }
                  Text {
                    width: parent.width
                    text: modelData.description
                    color: c.muted; font.pixelSize: 12; wrapMode: Text.Wrap
                    maximumLineCount: open ? 40 : 2; elide: Text.ElideRight
                  }
                  Text {
                    visible: open
                    width: parent.width
                    text: {
                      var p = (modelData.parameters || {}).properties || {}
                      var req = (modelData.parameters || {}).required || []
                      var ks = Object.keys(p)
                      if (!ks.length) return "no arguments"
                      return ks.map(function(k) { return "• " + k + (req.indexOf(k) >= 0 ? "*" : "") + "  " + (p[k].description || p[k].type || "") }).join("\n")
                    }
                    color: c.chrome; font.family: c.mono; font.pixelSize: 11; wrapMode: Text.Wrap
                  }
                  Text {
                    visible: open
                    text: {
                      var s = modelData.reclassified
                        ? "CLASSIFIED " + modelData.risk.toUpperCase() + "  ·  BUILT-IN " + modelData.default_risk.toUpperCase()
                        : modelData.risk.toUpperCase()
                      return s + (modelData.enabled ? "" : "  ·  DISABLED")
                    }
                    color: shell.riskColor(modelData.risk); font.family: c.mono; font.pixelSize: 10; font.letterSpacing: 1
                  }
                }
              }
            }
          }
        }

        // ============ CHAT
        Pane_ {
          Layout.fillWidth: true
          Layout.minimumWidth: 280
          Layout.fillHeight: true
          title: "CHAT"
          ColumnLayout {
            anchors.fill: parent
            spacing: 10
            ListView {
              id: chat
              Layout.fillWidth: true
              Layout.fillHeight: true
              clip: true
              spacing: 14
              model: { shell.rev; return shell.turns.length }
              onCountChanged: positionViewAtEnd()
              ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }
              delegate: Column {
                required property int index
                readonly property var turn: { shell.rev; return shell.turns[index] }
                width: chat.width - 10
                spacing: 6
                // you
                Rectangle {
                  anchors.right: parent.right
                  width: Math.min(youM.width + 26, parent.width * 0.8)
                  TextMetrics { id: youM; text: turn ? turn.query : ""; font.pixelSize: 14 }
                  height: youText.implicitHeight + 16
                  radius: 6; color: c.panelHi; border.color: index === shell.shownIndex ? c.chrome : c.line
                  Text { id: youText; x: 12; y: 8; width: parent.width - 24; text: turn ? turn.query : ""; color: c.text; wrapMode: Text.Wrap; font.pixelSize: 14 }
                  MouseArea { anchors.fill: parent; onClicked: shell.selected = index }
                }
                Text {
                  anchors.right: parent.right
                  text: turn ? (turn.at + (turn.source ? "  ·  " + turn.source : "")) : ""
                  color: c.muted; font.family: c.mono; font.pixelSize: 10
                }
                // arc
                Rectangle {
                  width: Math.min(Math.max(arcM.width, 120) + 26, parent.width * 0.85)
                  TextMetrics { id: arcM; text: arcText.text; font.pixelSize: 14 }
                  height: arcText.implicitHeight + 16
                  radius: 6; color: c.panelHi; border.color: turn && turn.done ? c.chromeDim : c.chrome
                  Text {
                    id: arcText; x: 12; y: 8; width: parent.width - 24
                    text: !turn ? "" : turn.done ? turn.reply : ("working" + ".".repeat(1 + (dots.tick % 3)) + "  " + turn.steps.length + " step" + (turn.steps.length === 1 ? "" : "s"))
                    color: turn && turn.done ? c.text : c.magenta; wrapMode: Text.Wrap; font.pixelSize: 14
                  }
                  MouseArea { anchors.fill: parent; onClicked: shell.selected = index }
                }
                Text {
                  visible: turn && turn.done
                  text: turn ? ("arc  ·  " + (turn.ms / 1000).toFixed(1) + "s  ·  " + turn.steps.filter(function(s) { return s.kind === "tool" }).length + " tool calls") : ""
                  color: c.muted; font.family: c.mono; font.pixelSize: 10
                }
              }
              Text {
                anchors.centerIn: parent
                visible: shell.turns.length === 0
                text: "Nothing yet.\nSay \"hey arc\" or type below."
                horizontalAlignment: Text.AlignHCenter
                color: c.muted; font.family: c.mono; font.pixelSize: 13
              }
            }
            RowLayout {
              Layout.fillWidth: true
              spacing: 8
              Field {
                id: input
                Layout.fillWidth: true
                placeholder: "ask arc anything — e.g. create a python calculator with tests"
                onAccepted: function(t) { shell.ask(t); clear() }
              }
              Button_ { label: "SEND"; onClicked: { shell.ask(input.value); input.clear() } }
            }
          }
        }

        // ============ TRACE
        Pane_ {
          Layout.preferredWidth: Math.max(300, win.width * 0.34)
          Layout.fillHeight: true
          title: "THOUGHT PROCESS" + (shell.shownTurn ? "  ·  #" + (shell.shownIndex + 1) : "")
          ListView {
            id: trace
            anchors.fill: parent
            clip: true
            spacing: 0
            model: { shell.rev; return shell.shownTurn ? shell.shownTurn.steps.length + 2 : 0 }
            onCountChanged: if (shell.selected < 0) positionViewAtEnd()
            ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }
            delegate: Item {
              required property int index
              readonly property var turn: shell.shownTurn
              readonly property bool isHead: index === 0
              readonly property bool isTail: turn && index === turn.steps.length + 1
              readonly property var step: { shell.rev; return turn && !isHead && !isTail ? turn.steps[index - 1] : null }
              readonly property color dot: isHead ? c.chrome
                : isTail ? (turn.done ? c.green : c.magenta)
                : step.kind === "thought" ? c.magenta
                : step.kind === "confirm" ? c.amber
                : step.kind === "error" ? c.red
                : step.kind === "hermes" ? c.chrome
                : step.status === "running" ? c.chrome
                : step.status === "success" ? c.green
                : step.status === "awaiting_confirmation" ? c.amber : c.red
              width: trace.width - 10
              height: body.implicitHeight + 18

              Rectangle { x: 11; width: 2; height: parent.height; color: c.line; visible: !isTail }
              Rectangle { x: 6; y: 4; width: 12; height: 12; radius: 6; color: c.bg; border.color: dot; border.width: 2
                Rectangle { anchors.centerIn: parent; width: 4; height: 4; radius: 2; color: dot } }

              Column {
                id: body
                x: 30; y: 2; width: parent.width - 34; spacing: 4
                Text {
                  text: isHead ? "REQUEST"
                    : isTail ? (turn.done ? "REPLY" : "WAITING")
                    : step.kind === "thought" ? "REASONING  ·  round " + step.round
                    : step.kind === "hermes" ? "HERMES  ·  step " + step.steps + (step.done ? "  ·  done" : "  ·  " + step.elapsed_s + "s")
                    : step.kind === "confirm" ? "NEEDS CONFIRMATION"
                    : step.kind === "error" ? "ERROR"
                    : "TOOL  ·  " + step.tool + "  ·  " + (step.status === "running" ? "running…"
                        : step.status === "awaiting_confirmation" ? "held"
                        : step.status + (step.ms ? "  " + (step.ms / 1000).toFixed(1) + "s" : ""))
                  width: parent.width; elide: Text.ElideRight
                  color: dot; font.family: c.mono; font.pixelSize: 11; font.bold: true; font.letterSpacing: 1
                }
                Text {
                  width: parent.width; wrapMode: Text.Wrap; color: c.text; font.pixelSize: 13
                  visible: text.length > 0
                  text: isHead ? turn.query
                    : isTail ? (turn.done ? turn.reply : "")
                    : step.kind === "thought" ? step.reasoning
                    : step.kind === "confirm" ? step.text
                    : step.kind === "error" ? step.text
                    : ""
                }
                Column {  // Hermes' live tool tail
                  visible: step !== null && step.kind === "hermes"
                  width: parent.width; spacing: 1
                  Repeater {
                    model: step && step.kind === "hermes" ? step.items.slice(-6) : []
                    delegate: Text {
                      required property string modelData
                      width: parent.width; elide: Text.ElideRight; wrapMode: Text.Wrap
                      text: "  " + modelData
                      color: c.muted; font.family: c.mono; font.pixelSize: 11
                    }
                  }
                }
                Text {  // model's working note alongside a tool call
                  width: parent.width; wrapMode: Text.Wrap; color: c.muted; font.pixelSize: 12; font.italic: true
                  visible: step !== null && step.kind === "thought" && step.text.length > 0
                  text: step && step.kind === "thought" ? "“" + step.text + "”" : ""
                }
                Rectangle {  // tool args + result
                  visible: step !== null && step.kind === "tool"
                  width: parent.width
                  height: visible ? toolBody.implicitHeight + 12 : 0
                  radius: 4; color: c.panelHi; border.color: c.line
                  Column {
                    id: toolBody; x: 8; y: 6; width: parent.width - 16; spacing: 4
                    Text {
                      width: parent.width; wrapMode: Text.WrapAnywhere; color: c.chrome; font.family: c.mono; font.pixelSize: 11
                      text: step && step.kind === "tool" ? "args  " + JSON.stringify(step.args || {}) : ""
                    }
                    Text {
                      width: parent.width; wrapMode: Text.WrapAnywhere; color: c.muted; font.family: c.mono; font.pixelSize: 11
                      visible: text.length > 0; maximumLineCount: 8; elide: Text.ElideRight
                      text: step && step.kind === "tool" && step.summary ? "→ " + step.summary : ""
                    }
                    // A caution-classified tool ran without a prompt, and the
                    // user should be able to see that afterwards rather than
                    // wondering why nothing asked.
                    Text {
                      width: parent.width; wrapMode: Text.Wrap; color: c.amber; font.family: c.mono; font.pixelSize: 11
                      visible: step !== null && step.kind === "tool" && !!step.warning
                      text: step && step.kind === "tool" && step.warning ? "⚠ " + step.warning : ""
                    }
                  }
                }
                Row {
                  visible: step !== null && step.kind === "confirm" && !step.resolved
                  spacing: 8
                  Button_ { label: "APPROVE"; tint: c.green; onClicked: shell.decide(step, true) }
                  Button_ { label: "REJECT"; tint: c.red; onClicked: shell.decide(step, false) }
                }
              }
            }
            Text {
              anchors.centerIn: parent
              visible: !shell.shownTurn
              text: "Ask something and the reasoning,\ntool calls and results show up here."
              horizontalAlignment: Text.AlignHCenter
              color: c.muted; font.family: c.mono; font.pixelSize: 12
            }
          }
        }
      }
    }
    Timer { id: dots; property int tick: 0; interval: 450; repeat: true; running: shell.turns.length > 0 && !shell.turns[shell.turns.length - 1].done; onTriggered: tick++ }
  }

  // ------------------------------------------------------------ components
  // The bubble: a soft blob that moves and wobbles with the song.
  //
  // Drawn on a canvas rather than animated with Rectangle items. A wobbling
  // circle built from scaled rectangles looks like a rounded rectangle being
  // resized, and fifty of them per frame is fifty items to lay out; one
  // canvas path is one item and one repaint.
  //
  // What makes it *move with the song* rather than just move: every term in
  // the shape and every term in the path is a function of `progress`, the
  // playhead as a fraction of the track. The blob's size, its wobble phase
  // and where it drifts along its arc are all driven by that, so the same
  // moment in a track always draws the same bubble, seeking there jumps it,
  // and two tracks do not look alike. `phase` adds a free-running wobble on
  // top so it breathes even while paused on the same second.
// The visualiser: a circular audio spectrum, fixed in one spot.
  //
  // A circle of thin radial bars in three concentric bands, radiating from a
  // small solid hub, with one continuous ring at the rim. It is a *spectrum*,
  // not a blob: the silhouette is always circular, the hub never moves, and
  // only the bar lengths change.
  //
  // What was tried and was wrong: a wobbling outline whose radius varied by
  // angle, drifting around an arc. At a low lobe count that reads as a
  // deformed egg, not a circle -- the vision check scored the first version at
  // 40/100 circular for exactly this reason. So the shape is now built from
  // bars around a fixed circle rather than from a single wandering outline.
  //
  // Drawn on one canvas: ~400 line segments per frame is one item and one
  // repaint, where the same thing as items would be hundreds of them.
// The visualiser: a circular audio spectrum, fixed in one spot.
  //
  // A circle of thin radial bars in three concentric bands, radiating from a
  // small solid hub, with one continuous ring at the rim. It is a *spectrum*,
  // not a blob: the silhouette is always circular, the hub never moves, and
  // only the bar lengths change.
  //
  // What was tried and was wrong: a wobbling outline whose radius varied by
  // angle, drifting around an arc. At a low lobe count that reads as a
  // deformed egg, not a circle.
  //
  // Plain Rectangles, not a Canvas. Canvas is the obvious choice for ~288
  // radial segments and it does not work here: `Canvas.onPaint` never fires in
  // this Quickshell -- a probe with a bare Canvas and an explicit
  // requestPaint() reported 0 paints and logged no error, so the canvas would
  // have rendered once and silently sat there showing a blank rectangle. That
  // is the worst possible failure for this component, because a blank square
  // looks like a design choice. Rectangles are ~288 items at 60fps, which is
  // nothing, and they actually paint.
  component Visualiser_: Item {
    id: vis
    property real progress: 0     // 0..1 through the track
    property bool playing: false
    property bool paused: false

    // The centre never moves. Every dimension below is a fraction of this, so
    // the burst scales with its box and stays put inside it.
    readonly property real cx: width / 2
    readonly property real cy: height / 2
    readonly property real size: Math.min(width, height)
    readonly property real hub: size * 0.10
    readonly property real inner: size * 0.20
    readonly property real outer: size * 0.46

    readonly property int bars: 96
    readonly property int rings: 3

    // Band radii: three concentric bands between `inner` and `outer`.
    readonly property var bandInner: [0, 1, 2].map(function (i) {
      return inner + (outer - inner) * (i / rings)
    })
    readonly property var bandLen: [0, 1, 2].map(function (i) {
      return (outer - inner) / rings
    })

    // Bar length at one angle, -1..1.
    //
    // Summed sines over (angle, ring) rather than per-bar noise, so adjacent
    // bars agree and the result has the lobed structure of a real spectrum
    // instead of looking like static. The ring term offsets the bands so they
    // are not three copies of one silhouette.
    //
    // Coprime lobe counts and differing phase rates are load-bearing. Two terms
    // at the same rate hold their relationship for the whole track, which reads
    // as one shape pulsing rather than a spectrum changing -- measured at
    // "bars barely move" before this was fixed.
    function spectrumAt(angle, ring) {
      var t = vis.progress
      return Math.sin(angle * 2 + t * Math.PI * 2 + ring * 0.9) * 0.42
           + Math.sin(angle * 3 - t * Math.PI * 3.7 + ring * 1.7) * 0.30
           + Math.sin(angle * 5 + t * Math.PI * 5.3 + ring * 2.3) * 0.18
           + Math.sin(angle * 7 - t * Math.PI * 8.9 + ring * 3.1) * 0.10
    }

    // Bar length as a fraction of its band, 0.12..1.
    //
    // The 2.2 power expands the middle of the range. A linear map leaves most
    // bars within a few percent of the mean, which reads as a static ring even
    // though every bar technically changed. The floor of 0.12 stops any bar
    // collapsing to nothing: a spectrum with gaps in it looks broken, not quiet.
    function levelAt(angle, ring) {
      var l = 0.12 + 0.88 * Math.pow(vis.spectrumAt(angle, ring) * 0.5 + 0.5, 2.2)
      return vis.paused ? 0.12 + 0.88 * 0.35 * l : l
    }

    // Cool at the hub, hot at the rim: brightness grows outward so the edge of
    // the burst is the hottest part of it.
    function rampAt(depth) {
      var u = Math.max(0, Math.min(1, (depth - 0.4) / 0.6))
      if (vis.paused) return Qt.rgba(c.chromeDim.r, c.chromeDim.g, c.chromeDim.b, 0.5)
      if (depth < 0.4)
        return Qt.rgba(0.59, 0.75, 1.0, 0.55 + 0.35 * (depth / 0.4))
      return Qt.rgba(0.78 + 0.22 * u, 0.51 - 0.26 * u, 0.90 - 0.29 * u, 0.55 + 0.40 * u)
    }

    // --- the bars
    //
    // One Repeater over bars * rings rather than a Repeater per ring, so the
    // whole spectrum is a single flat list: `index` divides into band and
    // spoke, and the binding engine sees one model rather than three nested
    // ones.
    Repeater {
      model: vis.bars * vis.rings
      // A wrapper Item positioned AT the centre with its origin at its own
      // top-left, so rotating it pivots about the centre. The bar inside then
      // hangs off the top edge, extending outward along -y.
      //
      // Both halves of this are load-bearing, and both were wrong first:
      //
      //  - Rotating the bar directly pivots about the *bar's* origin, not the
      //    centre, so each bar swings in place and all 96 stack at 12 o'clock.
      //    That is what the first screenshot showed: one vertical chain of bars
      //    at the top of an otherwise empty ring.
      //  - Anchoring the bar's own inner end at r0 fails the same way by
      //    another route -- every inner end lands on the same point, so there
      //    is nothing for the rotation to sweep.
      //
      // A Rectangle, not an Item: the bar needs `color`, and an Item silently
      // drops the assignment, leaving nothing drawn but a load-time warning.
      delegate: Item {
        id: sp
        required property int index
        readonly property int ring: Math.floor(index / vis.bars)
        readonly property int spoke: index % vis.bars
        readonly property real a: (spoke / vis.bars) * Math.PI * 2
        readonly property real bandLen: vis.bandLen[ring]
        // Inset from the band edge so a bar never bleeds into the next band.
        readonly property real r0: vis.bandInner[ring] + bandLen * 0.06
        // Animated so the spectrum moves smoothly between 1 Hz playhead
        // updates rather than stepping. The duration is shorter than the poll
        // interval on purpose: a bar should finish its move well before the
        // next one starts, or the animation lags the playhead visibly.
        readonly property real length:
          r0 + (bandLen * 0.94) * vis.levelAt(a, ring)

        width: 0
        height: 0
        x: vis.cx
        y: vis.cy
        transformOrigin: Item.TopLeft
        rotation: sp.a * 180 / Math.PI

        Rectangle {
          // -length to -r0, so the bar spans radii r0..length and grows
          // outward from the centre.
          x: -width / 2
          y: -sp.length
          width: vis.size * 0.02
          height: Math.max(1, sp.length - sp.r0)
          color: vis.rampAt((sp.ring + vis.levelAt(sp.a, sp.ring)) / (vis.rings + 1))
          Behavior on height {
            NumberAnimation { duration: 220; easing.type: Easing.OutQuad }
          }
          Behavior on color {
            ColorAnimation { duration: 220 }
          }
        }
      }
    }

    // --- the resting rim and band guides
    //
    // The continuous ring at the outer edge is the fixed silhouette: every bar
    // moves, and this does not, so the burst reads as one object rather than a
    // cloud of loose spokes.
    Repeater {
      model: vis.rings + 1
      delegate: Rectangle {
        required property int index
        // The outermost ring is the rim and is drawn brighter and thicker; the
        // inner ones are faint guides, structure the bars are built on.
        readonly property real r: vis.inner + (vis.outer - vis.inner) * (index / vis.rings)
        readonly property bool isRim: index === vis.rings
        width: r * 2
        height: r * 2
        x: vis.cx - r
        y: vis.cy - r
        radius: r
        color: "transparent"
        border.width: isRim ? Math.max(1.5, vis.size * 0.016) : 1
        border.color: isRim
          ? Qt.rgba(c.magenta.r, c.magenta.g, c.magenta.b, vis.paused ? 0.45 : 0.9)
          : Qt.rgba(c.chrome.r, c.chrome.g, c.chrome.b, 0.16)
      }
    }

    // --- the hub
    //
    // A solid core with a halo, so the burst has a centre of gravity. Without
    // one the spokes read as floating debris.
    Rectangle {
      width: vis.hub * 2
      height: vis.hub * 2
      x: vis.cx - vis.hub
      y: vis.cy - vis.hub
      radius: vis.hub
      color: Qt.rgba(0.98, 0.94, 1.0, vis.paused ? 0.4 : 0.95)
    }
    Rectangle {
      width: vis.hub * 3.2
      height: vis.hub * 3.2
      x: vis.cx - vis.hub * 1.6
      y: vis.cy - vis.hub * 1.6
      radius: vis.hub * 1.6
      color: "transparent"
      border.width: 1
      border.color: Qt.rgba(c.magenta.r, c.magenta.g, c.magenta.b, 0.35)
    }
  }
  component Pane_: Rectangle {
    id: pane
    property string title: ""
    default property alias content: holder.data
    // The window surface, translucent so the desktop shows through. This was
    // a hardcoded rgba blue and was 56% of the window: the one place the old
    // palette survived the swap, and the reason the app still looked blue.
    color: Qt.rgba(c.panel.r, c.panel.g, c.panel.b, 0.94)
    border.color: c.line
    radius: 6
    // corner brackets
    Repeater {
      model: 4
      Item {
        required property int index
        width: 14; height: 14
        x: index % 2 ? pane.width - 14 : 0
        y: index < 2 ? 0 : pane.height - 14
        Rectangle { width: 14; height: 2; color: c.chrome; y: index < 2 ? 0 : 12 }
        Rectangle { width: 2; height: 14; color: c.chrome; x: index % 2 ? 12 : 0 }
      }
    }
    Text { x: 16; y: 10; text: "▍" + pane.title; color: c.chrome; font.family: c.mono; font.pixelSize: 11; font.bold: true; font.letterSpacing: 2 }
    Item { id: holder; anchors.fill: parent; anchors.margins: 12; anchors.topMargin: 34 }
  }

  component Tag: Rectangle {
    property string label: ""
    property color tint: c.chromeDim
    height: 24; width: tl.implicitWidth + 20; radius: 3
    color: "transparent"; border.color: tint
    Text { id: tl; anchors.centerIn: parent; text: parent.label; color: parent.tint; font.family: c.mono; font.pixelSize: 11; font.bold: true; font.letterSpacing: 1 }
  }

  // Per-tool safety classification: three cells, one click each.
  //
  // A segmented control rather than a ComboBox on purpose. The whole point of
  // the feature is that marking a read-only tool `dangerous` is a decision the
  // user should make with all three options visible, not by opening a menu and
  // scanning it. The tick marks the level the tool ships with, so an override
  // is visible as an override and not mistaken for a mistake in the code.
  component RiskPicker: Row {
    id: picker
    property string tool: ""
    property string current: "safe"     // effective level (what the runtime uses)
    property string builtin: "safe"     // level the tool ships with
    property bool reclassified: false
    signal picked(string level)
    spacing: 4
    // Dangerous-by-nature tools (reboot, shutdown, code) can be raised but
    // never lowered -- the daemon refuses it -- so offering the lower cells
    // would be offering a button that cannot work.
    property bool lowerable: true
    readonly property bool locked: builtin === "dangerous" && !lowerable

    Repeater {
      model: picker.locked ? [] : shell.levels
      delegate: Rectangle {
        required property string modelData
        readonly property bool active: modelData === picker.current
        readonly property color tint: shell.riskColor(modelData)
        width: lbl.implicitWidth + 16
        height: 20
        radius: 3
        color: active ? Qt.rgba(tint.r, tint.g, tint.b, 0.2) : cellMa.containsMouse ? c.panelHi : "transparent"
        border.color: active ? tint : cellMa.containsMouse ? Qt.rgba(tint.r, tint.g, tint.b, 0.6) : c.line
        opacity: picker.enabled ? 1 : 0.4
        Text {
          id: lbl
          anchors.centerIn: parent
          text: (modelData === "dangerous" ? "DANGER" : modelData.toUpperCase()) + (modelData === picker.builtin ? " ·" : "")
          color: active ? tint : c.muted
          font.family: c.mono; font.pixelSize: 9; font.bold: true; font.letterSpacing: 1
        }
        MouseArea {
          id: cellMa
          anchors.fill: parent
          hoverEnabled: true
          cursorShape: Qt.PointingHandCursor
          // Clicking the active cell clears an override.
          onClicked: picker.picked(active && picker.reclassified ? "default" : modelData)
        }
      }
    }
    Rectangle {
      visible: picker.locked
      width: lockLbl.implicitWidth + 16; height: 20; radius: 3
      color: "transparent"; border.color: c.red
      Text {
        id: lockLbl
        anchors.centerIn: parent
        text: "DANGER  ·  ALWAYS ASKS"
        color: c.red; font.family: c.mono; font.pixelSize: 9; font.bold: true; font.letterSpacing: 1
      }
    }
    Rectangle {
      visible: picker.reclassified && !picker.locked
      width: visible ? 20 : 0
      height: 20
      radius: 3
      color: resetMa.containsMouse ? c.panelHi : "transparent"
      border.color: c.line
      Text { anchors.centerIn: parent; text: "↺"; color: resetMa.containsMouse ? c.chrome : c.muted; font.family: c.mono; font.pixelSize: 12 }
      MouseArea {
        id: resetMa
        anchors.fill: parent
        hoverEnabled: true
        cursorShape: Qt.PointingHandCursor
        onClicked: picker.picked("default")
      }
    }
  }


  component Button_: Rectangle {
    id: btn
    property string label: ""
    property color tint: c.chrome
    signal clicked()
    height: 34; width: bl.implicitWidth + 28; radius: 4
    color: bma.containsMouse ? Qt.rgba(tint.r, tint.g, tint.b, 0.18) : "transparent"
    border.color: tint
    Text { id: bl; anchors.centerIn: parent; text: btn.label; color: btn.tint; font.family: c.mono; font.pixelSize: 12; font.bold: true; font.letterSpacing: 2 }
    MouseArea { id: bma; anchors.fill: parent; hoverEnabled: true; cursorShape: Qt.PointingHandCursor; onClicked: btn.clicked() }
  }

  // A transport button: a glyph, a colour, a tooltip on hover. Sized square so
  // the row of them lines up without each one measuring its own label.
  component Transport_: Rectangle {
    id: btn
    property string glyph: ""
    property string tip: ""
    property color tint: c.chrome
    property bool active: false
    signal clicked()
    implicitWidth: 28
    implicitHeight: 24
    radius: 3
    color: active
      ? Qt.rgba(tint.r, tint.g, tint.b, 0.22)
      : bma.containsMouse ? c.panelHi : "transparent"
    border.color: active ? tint : bma.containsMouse ? Qt.rgba(tint.r, tint.g, tint.b, 0.6) : c.line
    opacity: enabled ? 1 : 0.35
    Text {
      anchors.centerIn: parent
      text: btn.glyph
      color: btn.active ? btn.tint : bma.containsMouse ? c.text : c.muted
      font.pixelSize: 12
    }
    MouseArea {
      id: bma
      anchors.fill: parent
      hoverEnabled: true
      enabled: btn.enabled
      cursorShape: Qt.PointingHandCursor
      onClicked: btn.clicked()
    }
    Rectangle {
      visible: bma.containsMouse && btn.tip.length > 0
      anchors.bottom: parent.bottom
      anchors.horizontalCenter: parent.horizontalCenter
      anchors.bottomMargin: 30
      width: tipText.implicitWidth + 12
      height: 18
      radius: 3
      color: c.bg
      border.color: c.line
      z: 50
      Text {
        id: tipText
        anchors.centerIn: parent
        text: btn.tip
        color: c.muted
        font.family: c.mono; font.pixelSize: 10
      }
    }
  }

  component Field: Rectangle {
    id: fld
    property string placeholder: ""
    readonly property string value: ti.text
    signal accepted(string text)
    signal edited(string text)
    function clear() { ti.text = "" }
    implicitHeight: 34; implicitWidth: 200; radius: 4
    color: c.panelHi; border.color: ti.activeFocus ? c.chrome : c.line
    TextInput {
      id: ti
      anchors.fill: parent; anchors.leftMargin: 12; anchors.rightMargin: 12
      verticalAlignment: TextInput.AlignVCenter
      color: c.text; font.family: c.mono; font.pixelSize: 13; clip: true
      selectionColor: c.chromeDim
      onAccepted: fld.accepted(text)
      onTextEdited: fld.edited(text)
    }
    Text {
      anchors.fill: ti; verticalAlignment: Text.AlignVCenter
      visible: ti.text.length === 0; text: fld.placeholder
      elide: Text.ElideRight
      color: c.muted; font.family: c.mono; font.pixelSize: 13
    }
  }
}
