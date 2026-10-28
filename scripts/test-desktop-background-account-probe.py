#!/usr/bin/env python3
"""Opt-in saved-profile observer for the interactive background/quit check.
Run only after profile backup, with TeraRelay already fully quit.
It records process/window/mount state; it never prints credentials or file names.
"""
import argparse, json, os, subprocess, time
from pathlib import Path
os.umask(0o077)
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--backup", required=True)
parser.add_argument("--output-dir", required=True)
parser.add_argument("--binary", default="app/src-tauri/target/debug/terarelay")
args = parser.parse_args()
root = Path(args.output_dir).resolve()
root.mkdir(parents=True, exist_ok=True)
binary = str(Path(args.binary).resolve())
if not (Path(args.backup)/"app-data").is_dir():
    raise SystemExit("Saved-profile test backup is missing; stopped")
if subprocess.run(["pgrep", "-u", str(os.getuid()), "-x", "terarelay"], stdout=subprocess.DEVNULL).returncode == 0:
    raise SystemExit("Another TeraRelay is running; leaving it untouched")
env = os.environ.copy()
env.update(XDG_DATA_HOME=str(Path.home()/".local/share"), XDG_CONFIG_HOME=str(Path.home()/".config"),
           XDG_CACHE_HOME=str(Path.home()/".cache"), DBUS_SESSION_BUS_ADDRESS=f"unix:path=/run/user/{os.getuid()}/bus")
env.pop("TERARELAY_QA_FEATURE_A", None)
mount = Path.home()/"TeraRelay Drive"
def observe(app):
    mount_id = next((line.split()[0] for line in Path("/proc/self/mountinfo").read_text().splitlines()
        if line.split()[4] == str(mount).replace(" ", "\\040")), None)
    rows = subprocess.check_output(["wmctrl", "-lp"], text=True).splitlines()
    windows = [line.split()[0] for line in rows if len(line.split())>3 and line.split()[2]==str(app.pid)]
    return {"app_pid":app.pid, "alive":app.poll() is None, "visible_windows":windows, "mount_id":mount_id}
with (root/"background-account.log").open("w") as log:
    app = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
    try:
        while app.poll() is None:
            state = observe(app)
            temp = root/"background-account-state.tmp"
            temp.write_text(json.dumps(state, indent=2))
            os.replace(temp, root/"background-account-state.json")
            request = root/"background-reopen-request"
            if request.exists():
                request.unlink()
                subprocess.run([binary], env=env, stdout=log, stderr=subprocess.STDOUT, timeout=15, check=True)
            time.sleep(.2)
        (root/"background-account-exited.json").write_text(json.dumps(observe(app), indent=2))
    finally:
        if app.poll() is None:
            app.terminate()
            try: app.wait(timeout=15)
            except subprocess.TimeoutExpired: app.kill(); app.wait(timeout=5)
