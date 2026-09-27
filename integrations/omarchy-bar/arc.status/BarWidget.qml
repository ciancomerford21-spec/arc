// Arc assistant bar widget for the Omarchy shell.
//
// Event-driven, no polling: runs `arc bar --follow`, which prints one JSON
// line whenever Arc's state changes (and an "offline" line while arcd is
// down, reconnecting by itself). The process is restarted if it ever exits.
//
//   left click   toggle listening (push-to-talk)
//   right click  open (or focus) a live Arc activity log in a terminal
//   middle click stop speaking
//
// Settings (in shell.json, on the widget entry):
//   "arcCommand": path to the `arc` binary (default "arc")
//   "hideWhenOffline": hide the widget while arcd isn't running (default false)
import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

BarWidget {
  id: root
  moduleName: "arc.status"

  readonly property string arcCommand: String(setting("arcCommand", "arc"))
  readonly property bool hideWhenOffline: setting("hideWhenOffline", false) === true

  property string state: "offline"
  property string icon: "󰍭"   // mic-off until the daemon reports
  property string tooltip: "Arc daemon is not running"

  readonly property bool busy: state === "listening" || state === "thinking" || state === "executing" || state === "speaking"

  visible: !(hideWhenOffline && state === "offline")
  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  function arc(args) {
    Util.execArgv([root.arcCommand].concat(args))
  }

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
    id: follow
    command: [root.arcCommand, "bar", "--follow"]
    running: true
    stdout: SplitParser {
      onRead: function(line) { root.apply(line) }
    }
    onExited: function(code) {
      root.apply('{"state":"offline","tooltip":"arc bar exited (' + code + ')"}')
      restart.start()
    }
  }

  Timer {
    id: restart
    interval: 5000
    onTriggered: follow.running = true
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: root.icon
    slotSize: Style.bar.statusSlot
    fontSize: Style.font.caption
    active: root.busy
    dimmed: root.state === "offline"
    tooltipText: root.tooltip + "\nClick: talk · Right-click: activity · Middle: stop speaking"
    onPressed: function(b) {
      if (b === Qt.RightButton) Util.execArgv(["omarchy-launch-or-focus-tui", "--app-id=org.omarchy.arc", root.arcCommand, "watch"])
      else if (b === Qt.MiddleButton) root.arc(["voice", "stop-speaking"])
      else root.arc(["voice", "toggle"])
    }
  }
}
