#!/usr/bin/env bash
set -euo pipefail

CMD="${1:-}"
ARGS_JSON="${2:-{}}"

case "$CMD" in
  list_windows|focused_window)
    osascript -l JavaScript -e '
      const se = Application("System Events");
      const procs = se.applicationProcesses.whose({ backgroundOnly: false })();
      const wins = [];
      for (const p of procs) {
        const pname = p.name();
        const front = p.frontmost();
        let pWins = [];
        try { pWins = p.windows(); } catch { continue; }
        for (let i = 0; i < pWins.length; i++) {
          const w = pWins[i];
          try {
            const pos = w.position();
            const sz = w.size();
            wins.push({
              id: `${pname}:${i}`,
              window_id: `${pname}:${i}`,
              app_id: pname,
              wm_class: pname,
              title: w.name() || pname,
              focused: Boolean(front && i === 0),
              bounds: { x: pos[0], y: pos[1], width: sz[0], height: sz[1] }
            });
          } catch {}
        }
      }
      if ("'"$CMD"'" === "focused_window") {
        const fg = wins.find(w => w.focused) || null;
        JSON.stringify({ ok: Boolean(fg), backend: "macos-ax", focused_window: fg });
      } else {
        JSON.stringify({ ok: true, backend: "macos-ax", window_count: wins.length, windows: wins });
      }
    '
    ;;
  list_apps)
    osascript -l JavaScript -e '
      const se = Application("System Events");
      const names = se.applicationProcesses.whose({ backgroundOnly: false }).name();
      JSON.stringify({ ok: true, backend: "macos-ax", apps: names });
    '
    ;;
  activate_window)
    APP_NAME="$(node -e "const a=JSON.parse(process.argv[1]);console.log(a.app_id||a.title||'')" "$ARGS_JSON")"
    osascript -e "tell application \"$APP_NAME\" to activate"
    echo "{\"ok\":true,\"app_id\":\"$APP_NAME\"}"
    ;;
  screenshot)
    TMP_PNG="$(mktemp /tmp/smv_mac_XXXXXX.png)"
    trap 'rm -f "$TMP_PNG"' EXIT
    screencapture -x "$TMP_PNG"
    node -e '
      const fs = require("fs");
      const b64 = fs.readFileSync(process.argv[1]).toString("base64");
      console.log(JSON.stringify({ ok: true, format: "png", data_base64: b64 }));
    ' "$TMP_PNG"
    ;;
  click)
    swift -e '
      import CoreGraphics
      import Foundation
      let args = try! JSONSerialization.jsonObject(with: Data(CommandLine.arguments[1].utf8)) as! [String: Any]
      let x = (args["x"] as? NSNumber)?.doubleValue ?? 0
      let y = (args["y"] as? NSNumber)?.doubleValue ?? 0
      let pt = CGPoint(x: x, y: y)
      let down = CGEvent(mouseEventSource: nil, mouseType: .leftMouseDown, mouseCursorPosition: pt, mouseButton: .left)
      let up = CGEvent(mouseEventSource: nil, mouseType: .leftMouseUp, mouseCursorPosition: pt, mouseButton: .left)
      down?.post(tap: .cghidEventTap)
      up?.post(tap: .cghidEventTap)
    ' "$ARGS_JSON"
    echo "{\"ok\":true}"
    ;;
  type_text)
    TXT="$(node -e "const a=JSON.parse(process.argv[1]);console.log(a.text||'')" "$ARGS_JSON")"
    osascript -e "tell application \"System Events\" to keystroke $(node -e 'console.log(JSON.stringify(process.argv[1]))' "$TXT")"
    echo "{\"ok\":true}"
    ;;
  launch_app)
    APP="$(node -e "const a=JSON.parse(process.argv[1]);console.log(a.app||a.command||'')" "$ARGS_JSON")"
    open -a "$APP"
    echo "{\"ok\":true,\"app\":\"$APP\"}"
    ;;
  *)
    echo "{\"ok\":false,\"error\":\"Unsupported command on macOS: $CMD\"}"
    ;;
esac
