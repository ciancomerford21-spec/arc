// Arc assistant bar widget and panel for the Omarchy shell.
//
// One entry point, one bar icon, one panel. State comes from two streaming
// processes, both event-driven with no polling:
//
//   `arc bar --follow`               -> the icon, state, and tooltip
//   `arc watch --topics assistant`   -> what you said, what Arc said, and any
//                                       confirmation Arc is holding
//
// `arc watch` is the transcript source because bar.json only carries an icon
// glyph and a tooltip: there is no text in it to show. The human-readable
// lines ("heard: ...", "arc: ...") are the CLI's own rendering; the structured
// events in the same stream are used for confirmations, which need the id.
//
//   left click    open / close the panel
//   middle click  stop Arc speaking
//   right click   push-to-talk toggle
//
// Settings (in shell.json, on the widget entry):
//   "arcCommand": path to the `arc` binary (default "arc")
//   "hideWhenOffline": hide the widget while arcd isn't running (default false)
//   "historyLines": how many past turns to keep in the panel (default 40)
import QtQuick
import QtQuick.Controls
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

Panel {
  id: root
  moduleName: "arc.status"
  // IPC so `arc panel` can reach this without knowing where it is anchored.
  ipcTarget: "arc.status"
  manageIpc: true

  readonly property string arcCommand: String(setting("arcCommand", "arc"))
  readonly property bool hideWhenOffline: setting("hideWhenOffline", false) === true
  readonly property int historyLines: Math.max(4, Number(setting("historyLines", 40)))

  // ------------------------------------------------------------------ state

  property string state: "offline"
  property string icon: "󰍭"          // mic-off until the daemon reports
  property string tooltip: "Arc daemon is not running"
  readonly property bool busy:
    state === "listening" || state === "thinking" || state === "executing" || state === "speaking"

  // The transcript. Each entry is {who, text, at} so the panel can colour the
  // two sides apart without re-parsing anything.
  property var turns: []
  // The confirmation the daemon is currently holding, if any.
  property var pending: null
  property int _revision: 0

  function arc(args) {
    Util.execArgv([root.arcCommand].concat(args))
  }

  // -------------------------------------------------------------- bar feed

  function apply(line) {
    var s
    try {
      s = JSON.parse(line)
    } catch (e) {
      return
    }
    root.state = String(s.state || "offline")
    root.icon = String(s.text || root.icon)
    root.tooltip = String(s.tooltip || "")
  }

  Process {
    id: barFollow
    command: [root.arcCommand, "bar", "--follow"]
    running: true
    stdout: SplitParser {
      onRead: function(line) { root.apply(line) }
    }
    onExited: function(code) {
      root.apply('{"state":"offline","tooltip":"arc bar exited (' + code + ')"}')
      restartDelay.restart()
    }
  }

  Timer {
    id: restartDelay
    interval: 5000
    onTriggered: if (!barFollow.running) barFollow.running = true
  }

  // ------------------------------------------------------- transcript feed

  // Only while the panel is open. A closed panel gains nothing from a live
  // transcript, and `arc watch` is a socket connection worth not holding.
  Process {
    id: watch
    command: [root.arcCommand, "watch", "--topics", "assistant"]
    running: root.opened
    stdout: SplitParser {
      onRead: function(line) { root.consume(line) }
    }
    onExited: if (root.opened) watchRestart.restart()
  }

  Timer {
    id: watchRestart
    interval: 2000
    onTriggered: if (!watch.running) watch.running = true
  }

  // `arc watch` mixes two line shapes: a human-readable rendering for the
  // common events, and a JSON object for the rest. Both are handled here so
  // the panel shows a transcript and a confirmation row from one stream.
  function consume(line) {
    var text = String(line || "")
    if (text === "") return

    if (text.charAt(0) === "{") {
      var s
      try {
        s = JSON.parse(text)
      } catch (e) {
        return
      }
      var kind = String(s.event || "")
      if (kind === "confirmation_required") {
        var p = s.pending || {}
        root.pending = {
          id: String(p.id || ""),
          summary: String(p.explanation || p.tool || "an action")
        }
        push("arc", "Waiting for you to confirm: " + root.pending.summary)
        return
      }
      if (kind === "reply") {
        push("arc", String(s.text || ""))
        return
      }
      if (kind === "heard") {
        push("you", String(s.text || ""))
        return
      }
      // bar/state events are already handled by the bar feed.
      return
    }

    if (text.indexOf("heard: ") === 0) {
      push("you", text.slice(7))
    } else if (text.indexOf("arc:   ") === 0) {
      push("arc", text.slice(7))
    } else if (text.indexOf("arc: ") === 0) {
      push("arc", text.slice(5))
    }
    // "[thinking]" and friends carry no transcript text worth keeping.
  }

  function push(who, text) {
    var value = String(text || "").trim()
    if (value === "") return
    var next = root.turns.concat([{ who: who, text: value, at: Date.now() }])
    while (next.length > root.historyLines) next.shift()
    root.turns = next
    root._revision++
    Qt.callLater(function() { if (listView) listView.positionViewAtEnd() })
  }

  // ------------------------------------------------------------- confirm

  function answer(approve) {
    // The daemon has no "confirm the latest" request, so this is the path
    // `arc confirm` takes: an ask with yes/no from the ui source, which the
    // daemon resolves against its own pending action.
    root.arc(approve ? ["confirm"] : ["reject"])
    root.pending = null
  }

  function clearTurns() {
    root.turns = []
  }

  // ------------------------------------------------------------- visuals

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property color fontFamily: bar ? bar.fontFamily : Style.font.family

  function stateColour() {
    if (root.state === "listening") return Color.accent
    if (root.state === "thinking" || root.state === "executing") return Color.warning
    if (root.state === "speaking") return Color.success
    if (root.state === "error") return Color.error
    return root.dim
  }

  visible: !(hideWhenOffline && state === "offline")
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onOpenedChanged: if (opened) Qt.callLater(function() { if (keyCatcher) keyCatcher.forceActiveFocus() })

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.icon
    slotSize: Style.bar.statusSlot
    fontSize: Style.font.caption
    active: root.busy
    dimmed: root.state === "offline"
    tooltipText: root.tooltip + "\nClick: panel · Right-click: talk · Middle: stop speaking"
    onPressed: function(b) {
      if (b === Qt.RightButton) root.arc(["voice", "toggle"])
      else if (b === Qt.MiddleButton) root.arc(["voice", "stop-speaking"])
      else root.toggle()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(360))
    contentHeight: panel.fittedContentHeight(body.implicitHeight, Style.space(460))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent

      onCloseRequested: root.close()
      onActivateRequested: if (root.pending) root.answer(true)
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) {
        if (t === "y" || t === "Y") root.answer(true)
        else if (t === "n" || t === "N") root.answer(false)
        else if (t === "c" || t === "C") root.clearTurns()
      }

      Column {
        id: body
        width: parent.width
        spacing: Style.space(8)

        // ---------- header ----------
        Item {
          width: parent.width
          implicitHeight: Style.space(24)

          Text {
            id: title
            anchors.left: parent.left
            anchors.verticalCenter: parent.verticalCenter
            text: "Arc"
            color: root.foreground
            font.family: root.fontFamily
            font.pixelSize: Style.font.body
            font.bold: true
          }

          Rectangle {
            anchors.left: title.right
            anchors.leftMargin: Style.space(8)
            anchors.verticalCenter: parent.verticalCenter
            width: Style.space(8)
            height: width
            radius: width / 2
            color: root.stateColour()
          }

          Text {
            anchors.right: parent.right
            anchors.verticalCenter: parent.verticalCenter
            text: root.state
            color: root.dim
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
          }
        }

        PanelSeparator {}

        // ---------- confirmation ----------
        Column {
          width: parent.width
          spacing: Style.space(8)
          visible: root.pending !== null

          Text {
            width: parent.width
            text: root.pending ? root.pending.summary : ""
            color: root.foreground
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.WordWrap
          }

          Row {
            spacing: Style.space(8)

            Button {
              text: "Approve"
              onClicked: root.answer(true)
            }

            Button {
              text: "Reject"
              onClicked: root.answer(false)
            }
          }
        }

        // ---------- transcript ----------
        ListView {
          id: listView
          width: parent.width
          height: Math.max(Style.space(80), Math.min(turnsHeight, Style.space(260)))
          clip: true
          spacing: Style.space(6)
          boundsBehavior: Flickable.StopAtBounds
          model: root.turns

          // Height the list to its contents, up to the cap above, so a short
          // conversation does not leave a tall empty box.
          property int turnsHeight: root._revision > 0 ? count * (Style.space(34) + Style.space(6)) : 0

          ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

          delegate: Text {
            required property var modelData
            width: listView.width
            text: (modelData.who === "you" ? "you  " : "arc  ") + modelData.text
            color: modelData.who === "you" ? root.dim : root.foreground
            font.family: root.fontFamily
            font.pixelSize: Style.font.caption
            wrapMode: Text.WordWrap
            elide: Text.ElideRight
            maximumLineCount: 3
          }
        }

        Text {
          width: parent.width
          visible: listView.count === 0
          text: "Nothing yet. Say \"hey arc\", or type with `arc ask`."
          color: root.dim
          font.family: root.fontFamily
          font.pixelSize: Style.font.caption
          horizontalAlignment: Text.AlignHCenter
          wrapMode: Text.WordWrap
        }
      }
    }
  }
}
