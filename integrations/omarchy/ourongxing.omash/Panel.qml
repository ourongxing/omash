pragma ComponentBehavior: Bound

import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

Panel {
  id: root
  moduleName: "ourongxing.omash"
  ipcTarget: "ourongxing.omash"
  manageIpc: false

  property bool online: false
  property string mode: ""
  property var groups: []
  property var delays: ({})
  property string selectedGroupName: ""
  property string errorMessage: ""

  readonly property color contentForeground: root.bar ? root.bar.foreground : Color.foreground
  readonly property string contentFontFamily: root.bar ? root.bar.fontFamily : Style.font.family
  readonly property int refreshIntervalMs: Math.max(2, Number(setting("refreshIntervalSec", 5))) * 1000
  readonly property var activeGroup: {
    for (var i = 0; i < root.groups.length; i++) {
      if (String(root.groups[i].name) === root.selectedGroupName) return root.groups[i]
    }
    return null
  }

  function refresh() {
    if (!stateProc.running) stateProc.running = true
  }

  function applyState(raw) {
    try {
      var next = JSON.parse(raw || "{}")
      root.online = next.online === true
      root.mode = String(next.mode || "")
      root.groups = Array.isArray(next.groups) ? next.groups : []
      root.delays = next.delays && typeof next.delays === "object" ? next.delays : ({})
      root.errorMessage = String(next.error || "")

      var found = false
      for (var i = 0; i < root.groups.length; i++) {
        if (String(root.groups[i].name) === root.selectedGroupName) {
          found = true
          break
        }
      }
      if (!found) root.selectedGroupName = root.groups.length > 0 ? String(root.groups[0].name) : ""
    } catch (error) {
      root.online = false
      root.errorMessage = "Invalid omash status response"
    }
  }

  function runAction(args) {
    if (actionProc.running) return
    actionProc.command = ["omash", "bar"].concat(args)
    actionProc.running = true
  }

  function setMode(nextMode) {
    if (!root.online || root.mode === nextMode) return
    root.runAction(["mode", nextMode])
  }

  function selectProxy(group, proxy) {
    if (!root.online || !group || !proxy) return
    root.runAction(["proxy", String(group), String(proxy)])
  }

  function testLatency() {
    if (!root.online || !root.activeGroup || delayProc.running) return
    delayProc.groupName = String(root.activeGroup.name)
    delayProc.command = ["omash", "bar", "delay", delayProc.groupName]
    delayProc.running = true
  }

  function applyDelays(raw) {
    try {
      var result = JSON.parse(raw || "{}")
      var next = ({})
      for (var currentName in root.delays) next[currentName] = root.delays[currentName]
      var measured = result.delays && typeof result.delays === "object" ? result.delays : ({})
      for (var measuredName in measured) next[measuredName] = Number(measured[measuredName])
      root.delays = next
      root.errorMessage = ""
    } catch (error) {
      root.errorMessage = "Invalid latency response"
    }
  }

  IpcHandler {
    target: "ourongxing.omash"
    function open() { root.open(); root.refresh() }
    function close() { root.close() }
    function show() { root.open(); root.refresh() }
    function hide() { root.close() }
    function toggle() { root.toggle(); if (root.opened) root.refresh() }
    function refresh() { root.refresh() }
  }

  Process {
    id: stateProc
    command: ["omash", "bar", "state"]
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: root.applyState(text)
    }
  }

  Process {
    id: actionProc
    onExited: function(exitCode) { root.refresh() }
  }

  Process {
    id: delayProc
    property string groupName: ""
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: root.applyDelays(text)
    }
    onExited: function(exitCode) {
      if (exitCode !== 0) root.errorMessage = "Latency test failed"
      else root.refresh()
    }
  }

  Timer {
    interval: root.refreshIntervalMs
    running: true
    repeat: true
    triggeredOnStart: true
    onTriggered: root.refresh()
  }

  onOpenedChanged: if (opened) refresh()

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "󰄛"
    tooltipText: root.online
      ? "omash · " + root.mode.toUpperCase()
      : "omash · Offline"
    onPressed: function(mouseButton) {
      if (mouseButton === Qt.LeftButton) root.toggle()
      else root.refresh()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: button
    owner: root
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(430))
    contentHeight: panel.fittedContentHeight(Math.min(content.implicitHeight, Style.space(500)))

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }

      Flickable {
        id: scroll
        anchors.fill: parent
        contentWidth: width
        contentHeight: content.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        flickableDirection: Flickable.VerticalFlick

        Column {
          id: content
          width: scroll.width
          spacing: Style.space(6)

          Item {
            width: parent.width
            implicitHeight: Math.max(heroIcon.implicitHeight, heroText.implicitHeight)

            Text {
              id: heroIcon
              anchors.left: parent.left
              anchors.verticalCenter: parent.verticalCenter
              text: "󰄛"
              color: root.contentForeground
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.display
            }

            Column {
              id: heroText
              anchors.left: heroIcon.right
              anchors.leftMargin: Style.space(6)
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              spacing: Style.space(2)

              Text {
                width: parent.width
                text: "omash"
                color: root.contentForeground
                font.family: root.contentFontFamily
                font.pixelSize: Style.font.title
                font.bold: true
              }

              Text {
                width: parent.width
                text: root.online
                  ? root.mode.toUpperCase() + " · " + root.groups.length + " PROXY GROUPS"
                  : (root.errorMessage || "MIHOMO OFFLINE")
                color: root.contentForeground
                opacity: 0.62
                font.family: root.contentFontFamily
                font.pixelSize: Style.font.caption
                font.bold: true
                font.letterSpacing: 1.0
                elide: Text.ElideRight
              }
            }
          }

          PanelSeparator { foreground: root.contentForeground }

          Column {
            width: parent.width
            spacing: Style.space(3)

            PanelSectionHeader {
              text: "PROXY MODE"
              foreground: root.contentForeground
              fontFamily: root.contentFontFamily
            }

            Row {
              id: modeRow
              width: parent.width
              spacing: Style.space(3)

              Repeater {
                model: [
                  { id: "rule", label: "Rule" },
                  { id: "global", label: "Global" },
                  { id: "direct", label: "Direct" }
                ]

                Button {
                  required property var modelData
                  width: (modeRow.width - modeRow.spacing * 2) / 3
                  text: String(modelData.label)
                  foreground: root.contentForeground
                  fontFamily: root.contentFontFamily
                  horizontalPadding: Style.space(6)
                  verticalPadding: Style.space(1)
                  bordered: true
                  active: root.mode === modelData.id
                  enabled: root.online && !actionProc.running
                  onClicked: root.setMode(String(modelData.id))
                }
              }
            }
          }

          PanelSeparator { foreground: root.contentForeground }

          Column {
            id: groupsColumn
            width: parent.width
            spacing: Style.space(3)

            Item {
              width: parent.width
              implicitHeight: Math.max(groupsHeader.implicitHeight, latencyButton.implicitHeight)

              PanelSectionHeader {
                id: groupsHeader
                anchors.left: parent.left
                anchors.verticalCenter: parent.verticalCenter
                text: "PROXY GROUPS"
                foreground: root.contentForeground
                fontFamily: root.contentFontFamily
              }

              PanelActionButton {
                id: latencyButton
                anchors.right: parent.right
                anchors.verticalCenter: parent.verticalCenter
                size: Style.space(18)
                iconText: delayProc.running ? "…" : "󰓅"
                tooltipText: delayProc.running ? "Testing latency…" : "Test group latency"
                foreground: root.contentForeground
                fontFamily: root.contentFontFamily
                enabled: root.online && root.activeGroup !== null && !delayProc.running
                onClicked: root.testLatency()
              }
            }

            Text {
              visible: root.online && root.groups.length === 0
              width: parent.width
              text: "No selector groups"
              color: root.contentForeground
              opacity: 0.62
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.bodySmall
            }

            Row {
              id: groupBrowser
              width: parent.width
              spacing: Style.space(4)

              Rectangle {
                id: groupPane
                width: Math.floor((groupBrowser.width - groupBrowser.spacing) * 0.42)
                height: groupList.implicitHeight + Style.space(6)
                color: Style.normalFillFor(root.contentForeground, Color.accent)
                radius: Style.cornerRadius

                Column {
                  id: groupList
                  x: Style.space(5)
                  y: Style.space(3)
                  width: groupPane.width - Style.space(10)
                  spacing: 0

                  Repeater {
                    model: root.groups

                    Item {
                      id: groupDelegate
                      required property var modelData
                      readonly property bool selected: root.selectedGroupName === String(modelData.name)
                      width: groupList.width
                      height: implicitHeight
                      implicitHeight: Math.max(groupMarker.implicitHeight, groupLabel.implicitHeight, groupProxy.implicitHeight) + Style.space(1)

                      Text {
                        id: groupMarker
                        anchors.left: parent.left
                        anchors.verticalCenter: parent.verticalCenter
                        width: Style.space(12)
                        text: groupDelegate.selected ? "●" : ""
                        color: Color.accent
                        font.family: root.contentFontFamily
                        font.pixelSize: Style.font.caption
                      }

                      Text {
                        id: groupLabel
                        anchors.left: groupMarker.right
                        anchors.right: groupProxy.left
                        anchors.rightMargin: Style.space(2)
                        anchors.verticalCenter: parent.verticalCenter
                        text: String(groupDelegate.modelData.name)
                        color: groupDelegate.selected ? Color.accent : root.contentForeground
                        opacity: groupDelegate.selected || groupMouse.containsMouse ? 1 : 0.78
                        font.family: root.contentFontFamily
                        font.pixelSize: Style.font.subtitle
                        font.bold: groupDelegate.selected
                        elide: Text.ElideRight
                      }

                      Text {
                        id: groupProxy
                        anchors.right: parent.right
                        anchors.verticalCenter: parent.verticalCenter
                        width: Style.space(40)
                        text: String(groupDelegate.modelData.now || "")
                        color: groupDelegate.selected ? Color.accent : root.contentForeground
                        opacity: groupDelegate.selected ? 0.72 : 0.5
                        font.family: root.contentFontFamily
                        font.pixelSize: Style.font.caption
                        horizontalAlignment: Text.AlignRight
                        elide: Text.ElideRight
                      }

                      MouseArea {
                        id: groupMouse
                        anchors.fill: parent
                        hoverEnabled: true
                        cursorShape: Qt.PointingHandCursor
                        onClicked: root.selectedGroupName = String(groupDelegate.modelData.name)
                      }
                    }
                  }
                }
              }

              Flickable {
                id: proxyScroll
                width: groupBrowser.width - groupPane.width - groupBrowser.spacing
                height: groupPane.height
                contentWidth: width
                contentHeight: proxyList.implicitHeight
                clip: true
                interactive: contentHeight > height
                boundsBehavior: Flickable.StopAtBounds
                flickableDirection: Flickable.VerticalFlick

                Column {
                  id: proxyList
                  width: proxyScroll.width
                  spacing: 0

                  Repeater {
                    model: root.activeGroup ? (root.activeGroup.all || []) : []

                    Item {
                      id: proxyDelegate
                      required property var modelData
                    readonly property bool selected: root.activeGroup !== null
                      && String(root.activeGroup.now) === String(modelData)
                    readonly property int delay: Number(root.delays[String(modelData)] || 0)
                      width: proxyList.width
                      height: implicitHeight
                      implicitHeight: Math.max(proxyMarker.implicitHeight, proxyLabel.implicitHeight) + Style.space(1)
                      enabled: root.online && !actionProc.running

                      Text {
                        id: proxyMarker
                        anchors.left: parent.left
                        anchors.verticalCenter: parent.verticalCenter
                        width: Style.space(12)
                        text: proxyDelegate.selected ? "●" : ""
                        color: Color.accent
                        font.family: root.contentFontFamily
                        font.pixelSize: Style.font.caption
                      }

                      Text {
                        id: proxyLabel
                        anchors.left: proxyMarker.right
                        anchors.right: delayLabel.left
                        anchors.rightMargin: Style.space(4)
                        anchors.verticalCenter: parent.verticalCenter
                        text: String(proxyDelegate.modelData)
                        color: proxyDelegate.selected ? Color.accent : root.contentForeground
                        opacity: proxyDelegate.selected || proxyMouse.containsMouse ? 1 : 0.78
                        font.family: root.contentFontFamily
                        font.pixelSize: Style.font.body
                        font.bold: proxyDelegate.selected
                        elide: Text.ElideRight
                      }

                      Text {
                        id: delayLabel
                        anchors.right: parent.right
                        anchors.verticalCenter: parent.verticalCenter
                        width: Style.space(52)
                        text: delayProc.running && delayProc.groupName === root.selectedGroupName
                          ? "…"
                          : (proxyDelegate.delay > 0 ? proxyDelegate.delay + " ms" : "")
                        color: proxyDelegate.delay >= 800 ? Color.urgent : root.contentForeground
                        opacity: proxyDelegate.delay > 0 ? 0.72 : 0.5
                        font.family: root.contentFontFamily
                        font.pixelSize: Style.font.caption
                        horizontalAlignment: Text.AlignRight
                      }

                      MouseArea {
                        id: proxyMouse
                        anchors.fill: parent
                        enabled: proxyDelegate.enabled
                        hoverEnabled: true
                        cursorShape: Qt.PointingHandCursor
                        onClicked: root.selectProxy(root.activeGroup.name, proxyDelegate.modelData)
                      }
                    }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}
