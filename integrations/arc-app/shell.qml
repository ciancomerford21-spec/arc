// Arc — standalone desktop app (Quickshell).
//
// Three panes, all live, no polling:
//   TOOLS    every tool Arc can call, with its description and risk level
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
  QtObject {
    id: c
    readonly property color bg: "#070b12"
    readonly property color panel: "#0b1320"
    readonly property color panelHi: "#101b2c"
    readonly property color line: "#1b2c44"
    readonly property color cyan: "#35e6ff"
    readonly property color cyanDim: "#1d7f8f"
    readonly property color magenta: "#ff3fa4"
    readonly property color amber: "#ffb547"
    readonly property color green: "#4dffa6"
    readonly property color red: "#ff5468"
    readonly property color text: "#d6e6f5"
    readonly property color muted: "#6f86a3"
    readonly property string mono: "JetBrainsMono Nerd Font"
  }

  function riskColor(r) {
    return r === "dangerous" ? c.red : r === "caution" ? c.amber : c.green
  }

  // ------------------------------------------------------------ state
  property string arcState: "offline"
  property string model: ""
  property var tools: []
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
      hit.ms = r.duration_ms || 0
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
        g.strokeStyle = "#0f2236"; g.lineWidth = 1
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
          Rectangle { id: core; anchors.centerIn: parent; width: 14; height: 14; radius: 7; color: shell.arcState === "offline" ? c.red : c.cyan }
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
          Text { text: "A R C"; color: c.cyan; font.family: c.mono; font.pixelSize: 22; font.bold: true; font.letterSpacing: 4 }
          Text { text: shell.model || "connecting…"; color: c.muted; font.family: c.mono; font.pixelSize: 11 }
        }
        Item { Layout.fillWidth: true }
        Tag { label: shell.arcState.toUpperCase(); tint: shell.arcState === "offline" ? c.red : shell.arcState === "idle" ? c.cyanDim : c.magenta }
        Tag { label: shell.tools.length + " TOOLS"; tint: c.cyanDim }
        Tag { label: shell.turns.length + " TURNS"; tint: c.cyanDim }
      }
      Rectangle { Layout.fillWidth: true; height: 1; color: c.cyan; opacity: 0.35 }

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
                border.color: open ? c.cyanDim : c.line
                opacity: modelData.enabled ? 1 : 0.45
                Rectangle { width: 3; height: parent.height - 12; anchors.verticalCenter: parent.verticalCenter; x: 0; radius: 1; color: shell.riskColor(modelData.risk) }
                Column {
                  id: tcol
                  x: 12; y: 8; width: parent.width - 20; spacing: 4
                  Row {
                    spacing: 8
                    Text { text: modelData.name; color: c.text; font.family: c.mono; font.pixelSize: 13; font.bold: true }
                    Text { text: modelData.category; color: c.muted; font.family: c.mono; font.pixelSize: 10; anchors.baseline: parent.children[0].baseline }
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
                    color: c.cyan; font.family: c.mono; font.pixelSize: 11; wrapMode: Text.Wrap
                  }
                  Text {
                    visible: open
                    text: modelData.risk.toUpperCase() + (modelData.enabled ? "" : "  ·  DISABLED")
                    color: shell.riskColor(modelData.risk); font.family: c.mono; font.pixelSize: 10; font.letterSpacing: 1
                  }
                }
                MouseArea { id: ma; anchors.fill: parent; hoverEnabled: true; onClicked: parent.open = !parent.open }
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
                  radius: 6; color: "#12253b"; border.color: index === shell.shownIndex ? c.cyan : c.line
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
                  radius: 6; color: c.panelHi; border.color: turn && turn.done ? c.cyanDim : c.magenta
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
              readonly property color dot: isHead ? c.cyan
                : isTail ? (turn.done ? c.green : c.magenta)
                : step.kind === "thought" ? c.magenta
                : step.kind === "confirm" ? c.amber
                : step.kind === "error" ? c.red
                : step.status === "running" ? c.cyan
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
                Text {  // model's working note alongside a tool call
                  width: parent.width; wrapMode: Text.Wrap; color: c.muted; font.pixelSize: 12; font.italic: true
                  visible: step !== null && step.kind === "thought" && step.text.length > 0
                  text: step && step.kind === "thought" ? "“" + step.text + "”" : ""
                }
                Rectangle {  // tool args + result
                  visible: step !== null && step.kind === "tool"
                  width: parent.width
                  height: visible ? toolBody.implicitHeight + 12 : 0
                  radius: 4; color: "#081018"; border.color: c.line
                  Column {
                    id: toolBody; x: 8; y: 6; width: parent.width - 16; spacing: 4
                    Text {
                      width: parent.width; wrapMode: Text.WrapAnywhere; color: c.cyan; font.family: c.mono; font.pixelSize: 11
                      text: step && step.kind === "tool" ? "args  " + JSON.stringify(step.args || {}) : ""
                    }
                    Text {
                      width: parent.width; wrapMode: Text.WrapAnywhere; color: c.muted; font.family: c.mono; font.pixelSize: 11
                      visible: text.length > 0; maximumLineCount: 8; elide: Text.ElideRight
                      text: step && step.kind === "tool" && step.summary ? "→ " + step.summary : ""
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
    color: Qt.rgba(0.043, 0.075, 0.125, 0.92)
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
        Rectangle { width: 14; height: 2; color: c.cyan; y: index < 2 ? 0 : 12 }
        Rectangle { width: 2; height: 14; color: c.cyan; x: index % 2 ? 12 : 0 }
      }
    }
    Text { x: 16; y: 10; text: "▍" + pane.title; color: c.cyan; font.family: c.mono; font.pixelSize: 11; font.bold: true; font.letterSpacing: 2 }
    Item { id: holder; anchors.fill: parent; anchors.margins: 12; anchors.topMargin: 34 }
  }

  component Tag: Rectangle {
    property string label: ""
    property color tint: c.cyanDim
    height: 24; width: tl.implicitWidth + 20; radius: 3
    color: "transparent"; border.color: tint
    Text { id: tl; anchors.centerIn: parent; text: parent.label; color: parent.tint; font.family: c.mono; font.pixelSize: 11; font.bold: true; font.letterSpacing: 1 }
  }

  component Button_: Rectangle {
    id: btn
    property string label: ""
    property color tint: c.cyan
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
    color: "#081018"; border.color: ti.activeFocus ? c.cyan : c.line
    TextInput {
      id: ti
      anchors.fill: parent; anchors.leftMargin: 12; anchors.rightMargin: 12
      verticalAlignment: TextInput.AlignVCenter
      color: c.text; font.family: c.mono; font.pixelSize: 13; clip: true
      selectionColor: c.cyanDim
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
