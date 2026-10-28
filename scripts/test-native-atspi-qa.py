#!/usr/bin/env python3
"""Native AT-SPI accessibility checks of exactly ONE owned synthetic TeraRelay process.

The default snapshot is read-only. Optional Settings-dialog checks safely open
and close only that isolated QA window; no settings values are changed.
Must use the isolated display/session bus. Never reads normal Telegram profiles.
"""
import argparse
import json
import os
import subprocess
import time
from pathlib import Path

import gi
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi

parser = argparse.ArgumentParser()
parser.add_argument("--qa-pid", type=int, required=True)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--settings-dialog", action="store_true",
                    help="Open/close only the Settings dialog of the owned synthetic QA app")
parser.add_argument("--all-settings-tabs", action="store_true",
                    help="Read Settings General, Themes, Proxy, Sharing and About tabs without changing values")
parser.add_argument("--tab-cycle-audit", action="store_true",
                    help="Use only Tab and Shift+Tab in the focused, owned synthetic Settings dialog")
parser.add_argument("--xdotool-keyboard", action="store_true",
                    help="Use isolated X11 xdotool after verifying the owned QA window PID")
parser.add_argument("--join-channel-dialog", action="store_true",
                    help="Open/read/close only Join Channel in the verified synthetic QA window; never submit")
parser.add_argument("--remote-upload-dialog", action="store_true",
                    help="Open/read/close Upload from Link in a synthetic QA channel; never analyze, import or submit")
parser.add_argument("--channel-info-dialog", action="store_true",
                    help="Read and dismiss Channel Info for the owned Movies QA channel; never invite")
parser.add_argument("--version-stack-dialog", action="store_true",
                    help="Inspect Add Version for a synthetic Movies QA file; never upload or modify versions")
parser.add_argument("--version-stack-tab-cycle-xdotool", action="store_true",
                    help="Test Shift+Tab/Tab only in the owned synthetic Version Stack modal")
args = parser.parse_args()
if args.version_stack_tab_cycle_xdotool and not args.version_stack_dialog:
    raise SystemExit("REFUSED: Version Stack keyboard test requires synthetic dialog scope")
if args.xdotool_keyboard and not args.tab_cycle_audit:
    raise SystemExit("REFUSED: X11 keyboard input requires explicit QA Tab-cycle audit")
if (args.settings_dialog or args.all_settings_tabs or args.tab_cycle_audit
        or args.join_channel_dialog or args.remote_upload_dialog
        or args.channel_info_dialog or args.version_stack_dialog):
    qa_env = Path(f"/proc/{args.qa_pid}/environ").read_bytes().split(b"\0")
    if b"TERARELAY_QA_FEATURE_A=1" not in qa_env:
        raise SystemExit("REFUSED: dialog interaction requires exact synthetic QA-mode process")
if args.qa_pid <= 0:
    raise SystemExit("Refusing unspecified QA process")
Atspi.init()

def children(item):
    try:
        count = min(item.get_child_count(), 300)
        return [item.get_child_at_index(i) for i in range(count)]
    except Exception:
        return []

INTERACTIVE_ROLE_PARTS = (
    "button", "check box", "combo box", "entry", "text field", "link",
    "menu item", "spin button", "slider", "radio button", "switch",
    "toggle button", "password text", "color chooser", "page tab",
)

def interactive_role(role):
    return any(part in role.lower() for part in INTERACTIVE_ROLE_PARTS)

def details(item):
    try:
        name = (item.get_name() or "").strip()
        role = item.get_role_name() or ""
        states = item.get_state_set()
        focusable = bool(states and states.contains(Atspi.StateType.FOCUSABLE))
        focused = bool(states and states.contains(Atspi.StateType.FOCUSED))
        return {"role": role, "name": name[:120], "focusable": focusable, "focused": focused}
    except Exception:
        return None

def owned_descendant_pids():
    # GTK/WebKit may expose the accessibility application from a child process
    # rather than the parent's process ID. Accept only descendants of the
    # verified isolated QA app, never another desktop program.
    seen = {args.qa_pid}
    pending = [args.qa_pid]
    while pending and len(seen) < 150:
        parent = pending.pop()
        path = Path(f"/proc/{parent}/task/{parent}/children")
        try:
            descendants = [int(pid) for pid in path.read_text().split()]
        except (OSError, ValueError):
            continue
        for child in descendants:
            if child not in seen:
                seen.add(child)
                pending.append(child)
    return seen

def find_owned_app():
    desktop = Atspi.get_desktop(0)
    owned = owned_descendant_pids()
    available = []
    for app in children(desktop):
        try:
            pid = app.get_process_id()
            name = (app.get_name() or "")[:70]
            available.append({"pid": pid, "name": name})
            if pid in owned:
                return app, available
        except Exception:
            continue
    return None, available

def walk_owned():
    app, available = find_owned_app()
    if app is None:
        raise RuntimeError(
            "No AT-SPI application for the exact owned QA process/descendants; "
            + "available=" + json.dumps(available[:12])
        )
    pending = [(app, 0)]
    visited = 0
    nodes = []
    while pending and visited < 650:
        node, depth = pending.pop()
        visited += 1
        result = details(node)
        if result:
            nodes.append((node, result))
        if depth < 14:
            pending.extend((child, depth + 1) for child in reversed(children(node)))
    return nodes

def scan():
    return [info for _, info in walk_owned()]

def press_owned_button(label, focus_before=False):
    for node, info in walk_owned():
        if info["role"] == "push button" and info["name"] == label:
            if not info["focusable"] or node.get_n_actions() == 0:
                raise RuntimeError(f"Owned {label} control cannot be activated")
            if focus_before and not node.grab_focus():
                raise RuntimeError(f"Owned {label} control could not receive focus")
            if not node.do_action(0):
                raise RuntimeError(f"Owned {label} action returned false")
            return
    raise RuntimeError(f"Named {label} button missing from owned application")

def verify_settings_dialog(all_tabs=False, tab_cycle=False):
    press_owned_button("Settings", focus_before=True)
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        entries = walk_owned()
        dialogs = [(node, info) for node, info in entries if info["role"] == "dialog"]
        if dialogs:
            break
        time.sleep(0.25)
    else:
        raise RuntimeError("Settings opened but AT-SPI did not expose a dialog role")
    dialog, identity = dialogs[0]
    if identity["name"] != "Settings":
        raise RuntimeError(f"Settings dialog accessible name incorrect: {identity['name']!r}")
    # Opening Settings must move keyboard focus to a named element in the dialog,
    # rather than leaving focus behind the modal in the dashboard.
    focus_deadline = time.monotonic() + 4
    while time.monotonic() < focus_deadline:
        if any(info["name"] == "Close" and info["focused"]
               for _, info in walk_owned()):
            break
        time.sleep(0.15)
    else:
        raise RuntimeError("Opening Settings did not focus its named Close button")

    tab_cycle_result = None
    if tab_cycle:
        if not args.all_settings_tabs or not os.environ.get("DISPLAY"):
            raise RuntimeError("Tab-cycle audit requires the full private Settings run on X11")

        def focused_dialog_control():
            dialog_entries = [(node, info) for node, info in walk_owned() if info["role"] == "dialog"]
            if not dialog_entries:
                # WebKit may briefly detach the accessible subtree while
                # processing a synthesized key. Poll again before declaring a
                # keyboard focus escape or a closed dialog.
                return None
            stack = [(dialog_entries[0][0], 0)]
            while stack:
                item, depth = stack.pop()
                info = details(item)
                if info and info["focused"] and interactive_role(info["role"]):
                    return info
                if depth < 14:
                    stack.extend((child, depth + 1) for child in reversed(children(item)))
            return None

        def await_focus(expected, timeout=3):
            end_time = time.monotonic() + timeout
            while time.monotonic() < end_time:
                info = focused_dialog_control()
                if info and expected(info):
                    return info
                time.sleep(0.13)
            raise RuntimeError("Native Settings keyboard focus did not reach the expected dialog control")

        # Require verified QA focus before any key event. The alternate X11
        # injector is opt-in and is bound to an owned, isolated app window.
        if not any(info["name"] == "Close" and info["focused"] for _, info in walk_owned()):
            raise RuntimeError("Refusing synthetic keys without verified owned Close focus")
        keyboard_backend = "atspi"
        if args.xdotool_keyboard:
            owned = owned_descendant_pids()
            candidate_windows = []
            rows = subprocess.check_output(["wmctrl", "-lp"], text=True, timeout=5)
            for row in rows.splitlines():
                fields = row.split(None, 4)
                if len(fields) >= 5 and "TeraRelay" in fields[4]:
                    try:
                        window_pid = int(fields[2])
                    except ValueError:
                        continue
                    if window_pid in owned:
                        candidate_windows.append(fields[0])
            if len(candidate_windows) != 1:
                raise RuntimeError(
                    f"REFUSED: expected one owned TeraRelay window, got {len(candidate_windows)}"
                )
            xid = candidate_windows[0]
            subprocess.run(["xdotool", "windowactivate", "--sync", xid],
                           check=True, timeout=6)
            active = int(subprocess.check_output(["xdotool", "getactivewindow"],
                                                  text=True, timeout=5).strip())
            if active != int(xid, 16):
                raise RuntimeError("REFUSED: active X11 window is not the owned QA window")
            close_nodes = [(node, info) for node, info in walk_owned()
                           if info["name"] == "Close" and info["role"] == "push button"]
            if len(close_nodes) != 1 or not close_nodes[0][0].grab_focus():
                raise RuntimeError("REFUSED: cannot restore owned Close focus after window activation")
            await_focus(lambda info: info["name"] == "Close")
            keyboard_backend = "xdotool-owned-x11"

        def send_key(sequence):
            if keyboard_backend == "xdotool-owned-x11":
                active = int(subprocess.check_output(["xdotool", "getactivewindow"],
                                                      text=True, timeout=5).strip())
                if active != int(xid, 16):
                    raise RuntimeError("REFUSED: keyboard focus moved to a foreign X11 window")
                subprocess.run(["xdotool", "key", "--clearmodifiers", "--delay", "65",
                                "shift+Tab" if sequence == "shift-tab" else "Tab"],
                               check=True, timeout=6)
            elif sequence == "shift-tab":
                pressed = Atspi.generate_keyboard_event(0xffe1, "Shift_L",
                                                       Atspi.KeySynthType.PRESS)
                if not pressed:
                    raise RuntimeError("AT-SPI refused private Shift synthesis")
                try:
                    if not Atspi.generate_keyboard_event(0xff09, "Tab",
                                                        Atspi.KeySynthType.PRESSRELEASE):
                        raise RuntimeError("AT-SPI refused private Shift+Tab")
                finally:
                    Atspi.generate_keyboard_event(0xffe1, "Shift_L",
                                                  Atspi.KeySynthType.RELEASE)
            else:
                if not Atspi.generate_keyboard_event(0xff09, "Tab",
                                                    Atspi.KeySynthType.PRESSRELEASE):
                    raise RuntimeError("AT-SPI refused private Tab")

        send_key("shift-tab")
        wrapped = await_focus(lambda info: info["name"] != "Close", timeout=4)
        send_key("tab")
        await_focus(lambda info: info["name"] == "Close", timeout=4)
        tab_cycle_result = {"shift_tab_stayed_inside_dialog": True,
                            "shift_tab_target": wrapped["name"],
                            "tab_wrapped_back_to_close": True,
                            "keyboard_backend": keyboard_backend}

    def dialog_controls():
        # Reacquire the owned dialog after React changes tab panels and nodes.
        matches = [(node, info) for node, info in walk_owned() if info["role"] == "dialog"]
        if not matches:
            raise RuntimeError("Settings dialog unexpectedly disappeared")
        parent = matches[0][0]
        queue = [(parent, 0)]
        controls = []
        while queue and len(controls) < 350:
            node, depth = queue.pop()
            info = details(node)
            if info and interactive_role(info["role"]):
                controls.append(info)
            if depth < 14:
                queue.extend((child, depth + 1) for child in reversed(children(node)))
        return controls

    tab_details = {}
    for tab in (("General", "Themes", "Proxy", "Sharing", "About") if all_tabs else ("General",)):
        if tab != "General":
            press_owned_button(tab)
            time.sleep(0.75)
        controls = dialog_controls()
        unnamed = [item["role"] for item in controls if not item["name"]]
        if not controls or unnamed:
            raise RuntimeError(f"Settings {tab} tab has unnamed controls: {unnamed[:18]}")
        if not any(c["name"] == "Close" and c["role"] == "push button" for c in controls):
            raise RuntimeError(f"Settings {tab} tab lacks named Close button")
        tab_details[tab] = {"interactive_count": len(controls),
                            "named_count": len(controls) - len(unnamed),
                            "focusable_count": sum(c["focusable"] for c in controls)}
    controls = dialog_controls()
    # Close only this isolated settings modal; never activate destructive controls.
    press_owned_button("Close")
    close_deadline = time.monotonic() + 5
    while time.monotonic() < close_deadline:
        if not any(info["role"] == "dialog" for _, info in walk_owned()):
            break
        time.sleep(0.3)
    else:
        raise RuntimeError("Settings dialog did not close through its named Close button")
    # Restore the exact control that launched Settings after the modal unmounts.
    return_deadline = time.monotonic() + 3
    while time.monotonic() < return_deadline:
        if any(info["name"] == "Settings" and info["focused"]
               for _, info in walk_owned()):
            break
        time.sleep(0.15)
    else:
        raise RuntimeError("Closing Settings did not restore keyboard focus to its opener")
    return {"name": identity["name"], "interactive_count": tab_details["General"]["interactive_count"],
            "named_count": tab_details["General"]["named_count"],
            "focusable_count": tab_details["General"]["focusable_count"],
            "tabs": tab_details, "close_button_named": True, "close_action_works": True,
            "close_initially_focused": True, "focus_restored_to_opener": True,
            "tab_cycle": tab_cycle_result}

def verify_join_channel_dialog():
    # Opening and dismissing the synthetic invitation dialog cannot join a
    # Telegram channel: this probe never invokes Submit/Join Channel.
    if any(info["role"] == "dialog" for _, info in walk_owned()):
        raise RuntimeError("REFUSED: another owned dialog is already open")
    press_owned_button("Join TeraRelay Channel", focus_before=True)
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        matches = [(node, info) for node, info in walk_owned()
                   if info["role"] == "dialog" and info["name"] == "Join TeraRelay Channel"]
        if matches:
            break
        time.sleep(0.25)
    else:
        raise RuntimeError("Join Channel opened but was not exposed as a named native dialog")
    queue = [(matches[0][0], 0)]
    entries = []
    while queue and len(entries) < 220:
        node, depth = queue.pop()
        info = details(node)
        if info:
            entries.append(info)
        if depth < 13:
            queue.extend((child, depth + 1) for child in reversed(children(node)))
    if not any(info["name"] == "Channel invite" and info["focusable"] for info in entries):
        raise RuntimeError("Join Channel invite field is missing a focusable accessible name")
    if not any(info["name"] == "Close" and info["role"] == "push button" for info in entries):
        raise RuntimeError("Join Channel icon Close control is not named")
    press_owned_button("Close")
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if not any(info["role"] == "dialog" for _, info in walk_owned()):
            return {"name": "Join TeraRelay Channel", "invite_field_named": True,
                    "close_button_named": True, "close_action_works": True,
                    "join_action_not_invoked": True}
        time.sleep(0.25)
    raise RuntimeError("Join Channel Close action did not dismiss the owned modal")


def verify_remote_upload_dialog():
    # All actions are limited to navigation and Close in a fresh synthetic QA
    # profile. This intentionally never enters links, triggers Analyze or
    # creates an import job.
    if any(info["role"] == "dialog" for _, info in walk_owned()):
        raise RuntimeError("REFUSED: another owned dialog is already open")
    press_owned_button("Movies QA", focus_before=False)
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        if any(info["role"] == "push button" and info["name"] == "Upload from Link"
               for _, info in walk_owned()):
            break
        time.sleep(0.25)
    else:
        raise RuntimeError("Synthetic Movies QA channel did not expose Upload from Link")

    press_owned_button("Upload from Link", focus_before=True)
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        matches = [(node, info) for node, info in walk_owned()
                   if info["role"] == "dialog" and info["name"] == "Upload from Link"]
        if matches:
            break
        time.sleep(0.25)
    else:
        raise RuntimeError("Remote Upload opened but no named native dialog was exposed")
    queue = [(matches[0][0], 0)]
    found = []
    while queue and len(found) < 280:
        node, depth = queue.pop()
        info = details(node)
        if info:
            found.append(info)
        if depth < 13:
            queue.extend((child, depth + 1) for child in reversed(children(node)))
    if not any(info["name"] == "Links to import" and info["focusable"] for info in found):
        raise RuntimeError("Remote Upload links field has no focusable AT-SPI name")
    if not any(info["role"] == "push button" and info["name"] == "Close" for info in found):
        raise RuntimeError("Remote Upload is missing its named Close control")
    press_owned_button("Close")
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if not any(info["role"] == "dialog" for _, info in walk_owned()):
            return {"name": "Upload from Link", "links_field_named": True,
                    "close_button_named": True, "close_action_works": True,
                    "analyze_not_invoked": True, "remote_import_not_started": True}
        time.sleep(0.25)
    raise RuntimeError("Remote Upload Close did not dismiss the owned dialog")


def verify_channel_info_dialog():
    # This probe only uses the synthetic Movies QA channel and its Close
    # control. It NEVER activates Invite or any real Telegram action.
    if any(info["role"] == "dialog" for _, info in walk_owned()):
        raise RuntimeError("REFUSED: another dialog is already open")
    press_owned_button("Movies QA")
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        if any(info["role"] == "push button"
               and info["name"] == "Channel info for Movies QA"
               for _, info in walk_owned()):
            break
        time.sleep(0.2)
    else:
        raise RuntimeError("Synthetic Movies QA channel lacks a named Channel Info opener")

    press_owned_button("Channel info for Movies QA", focus_before=True)
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        dialogs = [(node, info) for node, info in walk_owned()
                   if info["role"] == "dialog" and info["name"] == "Movies QA"]
        if dialogs:
            break
        time.sleep(0.25)
    else:
        raise RuntimeError("Channel Info did not expose a named Movies QA dialog")

    close_name = "Close channel info"
    nodes = [(dialogs[0][0], 0)]
    controls = []
    while nodes and len(controls) < 240:
        node, depth = nodes.pop()
        info = details(node)
        if info and interactive_role(info["role"]):
            controls.append(info)
        if depth < 12:
            nodes.extend((child, depth + 1) for child in reversed(children(node)))
    unnamed = [info["role"] for info in controls if not info["name"]]
    if unnamed or not controls:
        raise RuntimeError(f"Channel Info has unnamed controls: {unnamed}")
    if not any(info["role"] == "push button" and info["name"] == close_name
               for info in controls):
        raise RuntimeError("Channel Info Close button lacks an accessible name")
    focused_deadline = time.monotonic() + 4
    while time.monotonic() < focused_deadline:
        if any(info["name"] == close_name and info["focused"] for _, info in walk_owned()):
            break
        time.sleep(0.15)
    else:
        raise RuntimeError("Channel Info did not focus its Close button")

    press_owned_button(close_name)
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if not any(info["role"] == "dialog" for _, info in walk_owned()):
            break
        time.sleep(0.2)
    else:
        raise RuntimeError("Channel Info did not close through its named Close button")
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        if any(info["role"] == "push button"
               and info["name"] == "Channel info for Movies QA" and info["focused"]
               for _, info in walk_owned()):
            break
        time.sleep(0.2)
    else:
        raise RuntimeError("Channel Info did not restore focus to its opener")
    return {"name": "Movies QA", "interactive_count": len(controls),
            "all_controls_named": True, "close_initially_focused": True,
            "close_action_works": True, "focus_restored_to_opener": True,
            "invite_not_invoked": True}


def verify_version_stack_dialog():
    # Only opens a synthetic, existing file's Add Version view; no upload,
    # create-stack, delete, or Telegram action is ever invoked.
    if any(info["role"] == "dialog" for _, info in walk_owned()):
        raise RuntimeError("REFUSED: another owned dialog is open")
    press_owned_button("Movies QA")
    more = "More actions for Interstellar.1080p.BluRay.mkv"
    deadline = time.monotonic() + 9
    while time.monotonic() < deadline:
        if any(info["name"] == more and info["role"] == "push button"
               for _, info in walk_owned()):
            break
        time.sleep(0.2)
    else:
        raise RuntimeError("Synthetic Movies QA file's More actions button not found")
    press_owned_button(more)
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if any(info["name"] == "Add version" and info["role"] == "push button"
               for _, info in walk_owned()):
            break
        time.sleep(0.2)
    else:
        raise RuntimeError("Synthetic file menu does not expose Add version")
    press_owned_button("Add version")
    deadline = time.monotonic() + 8
    dialog = None
    while time.monotonic() < deadline:
        matches = [(node, info) for node, info in walk_owned()
                   if info["role"] == "dialog" and "Interstellar" in info["name"]]
        if len(matches) == 1:
            dialog = matches[0]
            break
        time.sleep(0.2)
    if dialog is None:
        raise RuntimeError("Version Stack did not expose the expected named native dialog")
    queue = [(dialog[0], 0)]
    controls = []
    while queue and len(controls) < 180:
        node, depth = queue.pop()
        info = details(node)
        if info and interactive_role(info["role"]):
            controls.append(info)
        if depth < 13:
            queue.extend((child, depth + 1) for child in reversed(children(node)))
    unnamed = [info["role"] for info in controls if not info["name"]]
    if not controls or unnamed:
        raise RuntimeError(f"Add Version has unnamed controls: {unnamed}")
    for label in ("Close", "Upload new version", "Use existing TeraRelay file"):
        if not any(label in c["name"] for c in controls):
            raise RuntimeError(f"Add Version missing accessible {label!r}")
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        if any(info["name"] == "Close" and info["focused"]
               for _, info in walk_owned()):
            break
        time.sleep(0.15)
    else:
        raise RuntimeError("Add Version did not initially focus its Close button")
    version_tab_cycle = None
    if args.version_stack_tab_cycle_xdotool:
        if not os.environ.get("DISPLAY"):
            raise RuntimeError("REFUSED: no isolated X11 display for Version Stack keys")
        owned = owned_descendant_pids()
        windows = []
        for row in subprocess.check_output(["wmctrl", "-lp"], text=True, timeout=5).splitlines():
            fields = row.split(None, 4)
            if len(fields) >= 5 and "TeraRelay" in fields[4]:
                try:
                    if int(fields[2]) in owned:
                        windows.append(fields[0])
                except ValueError:
                    continue
        if len(windows) != 1:
            raise RuntimeError(f"REFUSED: expected one owned TeraRelay window, got {len(windows)}")
        xid = windows[0]
        subprocess.run(["xdotool", "windowactivate", "--sync", xid], check=True, timeout=6)

        def check_owned_window():
            active = int(subprocess.check_output(["xdotool", "getactivewindow"],
                                                 text=True, timeout=5).strip())
            if active != int(xid, 16):
                raise RuntimeError("REFUSED: X11 active window is not owned by synthetic TeraRelay")

        def focused_version_control():
            roots = [(node, info) for node, info in walk_owned()
                     if info["role"] == "dialog" and "Interstellar" in info["name"]]
            if not roots:
                return None
            pending = [(roots[0][0], 0)]
            while pending:
                node, depth = pending.pop()
                info = details(node)
                if info and info["focused"] and interactive_role(info["role"]):
                    return info
                if depth < 13:
                    pending.extend((child, depth + 1) for child in reversed(children(node)))
            return None

        def await_focus(expected, timeout=4):
            until = time.monotonic() + timeout
            while time.monotonic() < until:
                info = focused_version_control()
                if info and expected(info):
                    return info
                time.sleep(0.15)
            raise RuntimeError("Version Stack keyboard focus boundary was not observed")

        check_owned_window()
        close_nodes = [(node, info) for node, info in walk_owned()
                       if info["role"] == "push button" and info["name"] == "Close"]
        if len(close_nodes) != 1 or not close_nodes[0][0].grab_focus():
            raise RuntimeError("REFUSED: synthetic Version Stack Close is not focusable")
        await_focus(lambda info: info["name"] == "Close")
        check_owned_window()
        subprocess.run(["xdotool", "key", "--clearmodifiers", "--delay", "65", "shift+Tab"],
                       check=True, timeout=6)
        wrapped = await_focus(lambda info: info["name"] != "Close")
        check_owned_window()
        subprocess.run(["xdotool", "key", "--clearmodifiers", "--delay", "65", "Tab"],
                       check=True, timeout=6)
        await_focus(lambda info: info["name"] == "Close")
        version_tab_cycle = {
            "keyboard_backend": "xdotool-owned-x11",
            "shift_tab_target": wrapped["name"],
            "shift_tab_stayed_inside_dialog": True,
            "tab_wrapped_back_to_close": True,
        }
    press_owned_button("Close")
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if not any(info["role"] == "dialog" for _, info in walk_owned()):
            break
        time.sleep(0.2)
    else:
        raise RuntimeError("Add Version Close did not dismiss the owned dialog")
    deadline = time.monotonic() + 4
    while time.monotonic() < deadline:
        if any(info["name"] == more and info["role"] == "push button" and info["focused"]
               for _, info in walk_owned()):
            break
        time.sleep(0.15)
    else:
        raise RuntimeError("Add Version did not restore keyboard focus to its file-row opener")
    return {
        "name": dialog[1]["name"],
        "interactive_count": len(controls),
        "all_controls_named": True,
        "close_initially_focused": True,
        "close_action_works": True,
        "focus_restored_to_file_actions": True,
        "tab_cycle": version_tab_cycle,
        "version_change_not_invoked": True,
        "upload_not_invoked": True,
        "delete_not_invoked": True,
    }


# A fresh private DBus session may spend time starting the AT-SPI registry
# and attaching WebKit's accessibility bridge before exposing the app.
end = time.monotonic() + 25
report = {"pass": False, "pid": args.qa_pid}
while time.monotonic() < end:
    try:
        nodes = scan()
        interactive = [n for n in nodes if interactive_role(n["role"])]
        unnamed = [n["role"] for n in interactive if not n["name"]]
        focusable = sum(n["focusable"] for n in interactive)
        report = {
            "pass": bool(interactive and not unnamed),
            "pid": args.qa_pid,
            "visible_node_count": len(nodes),
            "interactive_count": len(interactive),
            "named_count": len(interactive) - len(unnamed),
            "focusable_count": focusable,
            "unnamed_roles": unnamed[:25],
            "roles": sorted({n["role"] for n in nodes})[:60],
            "named_controls": interactive[:35],
        }
        if interactive:
            break
    except Exception as error:
        report["error"] = str(error)[:240]
    time.sleep(0.6)

if report["pass"] and (args.settings_dialog or args.all_settings_tabs):
    try:
        report["settings_dialog"] = verify_settings_dialog(
            all_tabs=args.all_settings_tabs, tab_cycle=args.tab_cycle_audit
        )
    except Exception as error:
        report["pass"] = False
        report["settings_error"] = str(error)[:280]

if report["pass"] and args.join_channel_dialog:
    try:
        report["join_channel_dialog"] = verify_join_channel_dialog()
    except Exception as error:
        report["pass"] = False
        report["join_channel_error"] = str(error)[:280]

if report["pass"] and args.remote_upload_dialog:
    try:
        report["remote_upload_dialog"] = verify_remote_upload_dialog()
    except Exception as error:
        report["pass"] = False
        report["remote_upload_error"] = str(error)[:280]

if report["pass"] and args.channel_info_dialog:
    try:
        report["channel_info_dialog"] = verify_channel_info_dialog()
    except Exception as error:
        report["pass"] = False
        report["channel_info_error"] = str(error)[:280]

if report["pass"] and args.version_stack_dialog:
    try:
        report["version_stack_dialog"] = verify_version_stack_dialog()
    except Exception as error:
        report["pass"] = False
        report["version_stack_error"] = str(error)[:280]

args.output.parent.mkdir(parents=True, exist_ok=True)
args.output.write_text(json.dumps(report, indent=2), encoding="utf-8")
print(json.dumps({key: value for key, value in report.items() if key != "named_controls"}), flush=True)
if not report["pass"]:
    raise SystemExit(1)
