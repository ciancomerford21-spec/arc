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
  Timer { id: reconnect; interval: 3000; onTriggered: { watcher.running = true; toolLoader.running = true; statusLoader.running = true } }

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

    // faint grid backdrop
    Canvas {
      anchors.fill: parent
      opacity: 0.35
      onPaint: {
        var g = getContext("2d"); g.clearRect(0, 0, width, height)
        g.strokeStyle = c.line; g.lineWidth = 1
        for (var x = 0; x < width; x += 32) { g.beginPath(); g.moveTo(x + .5, 0); g.lineTo(x + .5, height); g.stroke() }
        for (var y = 0; y < height; y += 32) { g.beginPath(); g.moveTo(0, y + .5); g.lineTo(width, y + .5); g.stroke() }
      }
      onWidthChanged: requestPaint(); onHeightChanged: requestPaint()
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
      }
      Rectangle { Layout.fillWidth: true; height: 1; color: c.chrome; opacity: 0.35 }

      // ---------------------------------------------------- panes
      RowLayout {
        Layout.fillWidth: true
        Layout.fillHeight: true
        spacing: 12

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
