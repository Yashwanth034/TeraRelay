#!/usr/bin/env bash
# Runs only inside the test-only GitHub Actions xvfb-run + dbus-run-session
# environment. No installed TeraRelay app, login session, or user profile.
set -euo pipefail

cd "$(dirname "$0")/.."
test -n "${DISPLAY:-}" || { echo "BLOCKED: no isolated X11 display"; exit 2; }
test -n "${DBUS_SESSION_BUS_ADDRESS:-}" || { echo "BLOCKED: no isolated session bus"; exit 2; }
test -c /dev/fuse || { echo "BLOCKED: no usable FUSE character device"; exit 2; }
test -x app/src-tauri/target/debug/terarelay
command -v openbox
command -v wmctrl
command -v xdotool
command -v fusermount3

mkdir -p terarelay-ci-fixtures
openbox > terarelay-ci-fixtures/ci-native-window-manager.log 2>&1 &
wm_pid=$!
cleanup() {
  local original=$?
  trap - EXIT
  kill "$wm_pid" 2>/dev/null || true
  wait "$wm_pid" 2>/dev/null || true
  exit "$original"
}
trap cleanup EXIT

wm_ready=false
for attempt in $(seq 1 40); do
  if wmctrl -m 2>/dev/null | grep -q '^Name: Openbox'; then
    wm_ready=true
    break
  fi
  if ! kill -0 "$wm_pid" 2>/dev/null; then
    cat terarelay-ci-fixtures/ci-native-window-manager.log
    echo "BLOCKED: isolated Openbox window manager exited"
    exit 2
  fi
  sleep 0.25
done
if [ "$wm_ready" != true ]; then
  cat terarelay-ci-fixtures/ci-native-window-manager.log
  echo "BLOCKED: isolated Openbox window manager did not register"
  exit 2
fi
echo "PASS isolated X11 window manager and private session bus ready"

binary="$PWD/app/src-tauri/target/debug/terarelay"
run_native() {
  local name="$1"
  shift
  local task="$PWD/terarelay-ci-fixtures/$name"
  test ! -e "$task" || { echo "REFUSED: expected new, private task: $task"; exit 2; }
  timeout --signal=TERM --kill-after=10s 220s \
    python3 scripts/test-desktop-background-native.py "$binary" "$task" "$@"
  # Catch leaked test-owned mounts before starting another QA instance.
  if grep -F "$name/" /proc/self/mountinfo; then
    echo "FAIL: leaked private FUSE mount for $name"
    exit 1
  fi
  if pgrep -x terarelay >/dev/null; then
    echo "FAIL: leftover isolated TeraRelay process after $name"
    exit 1
  fi
}

run_native ci-native-fs --storage-audit --filesystem-audit
run_native ci-native-recovery --storage-audit --recovery-audit --native-recovery-audit --crash-audit
run_native ci-native-project --storage-audit --listener-audit --project-audit
run_native ci-native-posix --storage-audit --project-posix-audit
run_native ci-native-cache --cache-policy
run_native ci-native-accessibility --storage-audit --settings-tab-cycle-xdotool-audit
run_native ci-native-join-channel --storage-audit --join-channel-accessibility-audit
run_native ci-native-remote-upload --storage-audit --remote-upload-accessibility-audit
run_native ci-native-channel-info --storage-audit --channel-info-accessibility-audit
run_native ci-native-version-stack --storage-audit --version-stack-tab-cycle-xdotool-audit

python3 - <<'PY'
import json
from pathlib import Path

root = Path("terarelay-ci-fixtures")
expected = [
    ("ci-native-fs", 27),
    ("ci-native-recovery", 38),
    ("ci-native-project", 30),
    ("ci-native-posix", 35),
    ("ci-native-cache", 28),
    ("ci-native-accessibility", 29),
    ("ci-native-join-channel", 28),
    ("ci-native-remote-upload", 28),
    ("ci-native-channel-info", 28),
    ("ci-native-version-stack", 27),
]
mountinfo = Path("/proc/self/mountinfo").read_text()
for name, minimum in expected:
    result = json.loads((root / name / "result.json").read_text())
    assert result["pass_result"], (name, result.get("error"))
    assert len(result["checks"]) >= minimum, (name, len(result["checks"]))
    assert name not in mountinfo, f"Leaked private FUSE mount: {name}"
    print(f"PASS {name}: {len(result['checks'])} native checks; no owned mount leaked")

posix = root / "ci-native-posix"
prepare = json.loads((posix / "project-posix-prepare.json").read_text())
verify = json.loads((posix / "project-posix-verify.json").read_text())
assert prepare["pass_result"] and verify["pass_result"]
assert len(prepare["checks"]) >= 7 and len(verify["checks"]) >= 2
assert verify["owned_fixture_removed"]
print("PASS strict POSIX pre/post crash-restart semantics and fixture cleanup")

accessibility = json.loads((root / "ci-native-accessibility/native-accessibility-result.json").read_text())
assert accessibility["pass"]
assert accessibility["interactive_count"] >= 15
assert accessibility["named_count"] == accessibility["interactive_count"]
assert all(not (c["role"] == "push button" and c["name"].endswith("WORKSPACE"))
           for c in accessibility["named_controls"]), "Workspace identity must not be a disabled button"
dialog = accessibility["settings_dialog"]
assert dialog["name"] == "Settings"
assert dialog["close_button_named"] and dialog["close_action_works"]
assert dialog["close_initially_focused"] and dialog["focus_restored_to_opener"]
cycle = dialog["tab_cycle"]
assert cycle["keyboard_backend"] == "xdotool-owned-x11"
assert cycle["shift_tab_stayed_inside_dialog"] and cycle["tab_wrapped_back_to_close"]
assert cycle["shift_tab_target"] == "Done", cycle
min_counts = {"General": 25, "Themes": 12, "Proxy": 14, "Sharing": 8, "About": 9}
assert set(dialog["tabs"]) == set(min_counts), dialog["tabs"]
for tab, minimum in min_counts.items():
    controls = dialog["tabs"][tab]
    assert controls["interactive_count"] >= minimum, (tab, controls)
    assert controls["named_count"] == controls["interactive_count"], (tab, controls)
    print(f"PASS Settings {tab}: {controls['named_count']} accessible names")
print("PASS native AT-SPI workspace controls, all five Settings tabs and working Close button")

channel_a11y = json.loads((root / "ci-native-join-channel/native-accessibility-result.json").read_text())
assert channel_a11y["pass"]
channel_dialog = channel_a11y["join_channel_dialog"]
assert channel_dialog["name"] == "Join TeraRelay Channel"
assert channel_dialog["invite_field_named"] and channel_dialog["close_button_named"]
assert channel_dialog["close_action_works"] and channel_dialog["join_action_not_invoked"]
print("PASS native Join Channel dialog and labelled invite; no Telegram join action")

remote_a11y = json.loads((root / "ci-native-remote-upload/native-accessibility-result.json").read_text())
assert remote_a11y["pass"]
remote_dialog = remote_a11y["remote_upload_dialog"]
assert remote_dialog["name"] == "Upload from Link"
assert remote_dialog["links_field_named"] and remote_dialog["close_button_named"]
assert remote_dialog["close_action_works"]
assert remote_dialog["analyze_not_invoked"] and remote_dialog["remote_import_not_started"]
print("PASS native Remote Upload dialog/label/Close, without analyzing URLs or starting an import")

info_a11y = json.loads((root / "ci-native-channel-info/native-accessibility-result.json").read_text())
assert info_a11y["pass"]
info_dialog = info_a11y["channel_info_dialog"]
assert info_dialog["name"] == "Movies QA"
assert info_dialog["interactive_count"] >= 8 and info_dialog["all_controls_named"]
assert info_dialog["close_initially_focused"] and info_dialog["close_action_works"]
assert info_dialog["focus_restored_to_opener"] and info_dialog["invite_not_invoked"]
print("PASS real native Channel Info dialog, named controls and focus restoration without inviting")

version_a11y = json.loads((root / "ci-native-version-stack/native-accessibility-result.json").read_text())
assert version_a11y["pass"]
version_dialog = version_a11y["version_stack_dialog"]
assert version_dialog["name"] == "Interstellar"
assert version_dialog["interactive_count"] >= 4 and version_dialog["all_controls_named"]
assert version_dialog["close_initially_focused"] and version_dialog["close_action_works"]
assert version_dialog["focus_restored_to_file_actions"]
version_cycle = version_dialog["tab_cycle"]
assert version_cycle["keyboard_backend"] == "xdotool-owned-x11"
assert version_cycle["shift_tab_stayed_inside_dialog"] and version_cycle["tab_wrapped_back_to_close"]
assert version_cycle["shift_tab_target"].startswith("Use existing TeraRelay file"), version_cycle
assert version_dialog["version_change_not_invoked"] and version_dialog["upload_not_invoked"]
assert version_dialog["delete_not_invoked"]
print("PASS real native Version Stack add dialog, Close and file-row focus restoration without mutation")

capability = json.loads((root / "ci-native-project/project-capabilities.json").read_text())
assert capability["audit_finished"] and capability["owned_mount_directory_removed"]
hardlink = capability["capabilities"]["Hardlink creation"]
if not hardlink["supported"]:
    print("LIMITATION: hardlinks unsupported; project capability audit is NOT fully green")
else:
    print("PASS hardlink support tested by project capability audit")
PY
