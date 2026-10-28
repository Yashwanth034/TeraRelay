#!/usr/bin/env python3
"""Real Linux window-close/Drive/tray integration; run inside a private DBus session.

Usage: dbus-run-session -- python3 scripts/test-desktop-background-native.py BINARY OUTPUT_DIR
Requires a running isolated Vite server started as:
  VITE_TERA_QA_FEATURE_A=1 npm run dev -- --host 127.0.0.1 --port 1420 --strictPort
The runner itself sets TERARELAY_QA_FEATURE_A=1 in the private native app.
Without BOTH flags, frontend QA seeding never runs and --storage-audit times out.
Uses a fresh private QA profile, real FUSE, X11 window events and exported tray menu.
Never opens, resets or deletes the user's normal profile.
"""
import argparse, errno, json, os, signal, socket, sqlite3, subprocess, threading, time, traceback
from pathlib import Path
import gi
gi.require_version("Gtk", "3.0")
from gi.repository import Gio, GLib, Gtk

parser = argparse.ArgumentParser()
parser.add_argument("binary")
parser.add_argument("output_dir")
parser.add_argument("--cache-policy", action="store_true")
parser.add_argument("--accessibility-audit", action="store_true")
parser.add_argument("--settings-accessibility-audit", action="store_true")
parser.add_argument("--settings-tab-cycle-audit", action="store_true")
parser.add_argument("--settings-tab-cycle-xdotool-audit", action="store_true")
parser.add_argument("--join-channel-accessibility-audit", action="store_true")
parser.add_argument("--remote-upload-accessibility-audit", action="store_true")
parser.add_argument("--channel-info-accessibility-audit", action="store_true")
parser.add_argument("--version-stack-accessibility-audit", action="store_true")
parser.add_argument("--version-stack-tab-cycle-xdotool-audit", action="store_true")
parser.add_argument("--storage-audit", action="store_true")
parser.add_argument("--filesystem-audit", action="store_true")
parser.add_argument("--recovery-audit", action="store_true")
parser.add_argument("--performance-audit", action="store_true")
parser.add_argument("--crash-audit", action="store_true")
parser.add_argument("--listener-audit", action="store_true")
parser.add_argument("--project-audit", action="store_true")
parser.add_argument("--project-posix-audit", action="store_true")
parser.add_argument("--native-recovery-audit", action="store_true")
args = parser.parse_args()
root = Path(args.output_dir).resolve()
root.mkdir(parents=True, exist_ok=False)
profile = root / "profile"
env = os.environ.copy()
for name, directory in [("XDG_DATA_HOME", "data"), ("XDG_CONFIG_HOME", "config"), ("XDG_CACHE_HOME", "cache")]:
    env[name] = str(profile / directory)
env["TERARELAY_QA_FEATURE_A"] = "1"
binary = str(Path(args.binary).resolve())
mount = profile / "cache/io.terarelay.desktop/TeraRelay Drive"
env["TERARELAY_DRIVE_MOUNT_PATH"] = str(mount)
if args.cache_policy:
    data = profile / "data/io.terarelay.desktop"
    data.mkdir(parents=True, mode=0o700)
    with sqlite3.connect(data / "shares.db") as db:
        db.execute("CREATE TABLE transfer_queues (kind TEXT PRIMARY KEY, items TEXT NOT NULL)")
        db.executemany("INSERT INTO transfer_queues VALUES (?,?)", [("upload","[]"),("download","[]")])
    retired = data / "tdlib-turbo-files-v1-0"
    retired.mkdir()
    (retired / "cached.bin").write_bytes(bytes(1024 * 1024))
    protected = {}
    for relative in ["tdlib-fast-session-v4/keep.bin", "tdlib-turbo-session-v1-0/keep.bin",
                     "tdlib-fast-database-key-v4", "tdlib-fast-files-v4/partial.bin"]:
        path = data / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"protected-cache-fixture")
        protected[path] = path.read_bytes()
    blocks = profile / "cache/io.terarelay.desktop/drive/blocks/cache-fixture"
    blocks.mkdir(parents=True)
    for index in range(3):
        with (blocks / ("cipher-%016x.bin" % index)).open("wb") as f:
            f.truncate(16 * 1024 * 1024 + 45)
    (blocks / "active.tmp").write_bytes(b"pending cache write")
    stage = profile / "cache/io.terarelay.desktop/drive/staging/pending-fixture"
    stage.parent.mkdir(parents=True)
    stage.write_bytes(b"pending Drive write")
    protected[stage] = stage.read_bytes()
connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
items = []
checks = {}
result = {}
loop = GLib.MainLoop()

xml = """<node><interface name="org.kde.StatusNotifierWatcher">
<method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
<method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
<property name="RegisteredStatusNotifierItems" type="as" access="read"/>
<property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
<property name="ProtocolVersion" type="i" access="read"/>
<signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
<signal name="StatusNotifierHostRegistered"/>
</interface></node>"""
interface = Gio.DBusNodeInfo.new_for_xml(xml).interfaces[0]
def method(conn, sender, path, iface, name, parameters, invocation):
    service = parameters.unpack()[0]
    if name == "RegisterStatusNotifierItem":
        item = sender + service if service.startswith("/") else service + "/StatusNotifierItem"
        if item not in items:
            items.append(item)
        conn.emit_signal(None, path, iface, "StatusNotifierItemRegistered", GLib.Variant("(s)", (item,)))
    invocation.return_value(None)
def prop(conn, sender, path, iface, name):
    return {
        "RegisteredStatusNotifierItems": GLib.Variant("as", items),
        "IsStatusNotifierHostRegistered": GLib.Variant("b", True),
        "ProtocolVersion": GLib.Variant("i", 0),
    }[name]
connection.register_object("/StatusNotifierWatcher", interface, method, prop, None)
Gio.bus_own_name_on_connection(connection, "org.kde.StatusNotifierWatcher", Gio.BusNameOwnerFlags.NONE, None, None)

def wait_for(description, predicate, seconds=20):
    stop = time.monotonic() + seconds
    while time.monotonic() < stop:
        value = predicate()
        if value:
            checks[description] = True
            return value
        time.sleep(.1)
    raise AssertionError(description)

def window_for(pid):
    rows = subprocess.check_output(["wmctrl", "-lp"], text=True).splitlines()
    for row in rows:
        fields = row.split(None, 4)
        if len(fields) >= 5 and fields[2] == str(pid) and "TeraRelay" in fields[4]:
            return fields[0]
    return None

def mounted():
    escaped = str(mount).replace(" ", "\\040")
    return next((line.split()[0] for line in Path("/proc/self/mountinfo").read_text().splitlines() if line.split()[4] == escaped), None)

def mapped(window):
    p = subprocess.run(["xwininfo", "-id", window], capture_output=True, text=True)
    return p.returncode == 0 and "Map State: IsViewable" in p.stdout

def tray_menu():
    service, path = items[-1].split("/", 1)
    path = "/" + path
    response = connection.call_sync(service, path, "org.freedesktop.DBus.Properties", "Get",
        GLib.Variant("(ss)", ("org.kde.StatusNotifierItem", "Menu")), None,
        Gio.DBusCallFlags.NONE, 5000, None)
    menu_path = response.unpack()[0]
    response = connection.call_sync(service, menu_path, "com.canonical.dbusmenu", "GetLayout",
        GLib.Variant("(iias)", (0, -1, [])), None, Gio.DBusCallFlags.NONE, 5000, None)
    found = {}
    def visit(node):
        if isinstance(node, GLib.Variant): node = node.unpack()
        item_id, properties, children = node
        if "label" in properties: found[properties["label"].replace("_", "")] = item_id
        for child in children: visit(child)
    visit(response.unpack()[1])
    return service, menu_path, found

def activate(label):
    service, path, found = tray_menu()
    assert label in found, (label, found)
    connection.call_sync(service, path, "com.canonical.dbusmenu", "Event",
        GLib.Variant("(isvu)", (found[label], "clicked", GLib.Variant("i", 0), 0)),
        None, Gio.DBusCallFlags.NONE, 5000, None)

def owned_ipv4_listeners(pid):
    inodes = set()
    for fd in Path(f"/proc/{pid}/fd").iterdir():
        try:
            target = os.readlink(fd)
        except FileNotFoundError:
            continue
        if target.startswith("socket:["):
            inodes.add(target[8:-1])
    ports = []
    for line in Path(f"/proc/{pid}/net/tcp").read_text().splitlines()[1:]:
        fields = line.split()
        host, port = fields[1].split(":")
        if fields[3] == "0A" and fields[9] in inodes and host == "0100007F":
            ports.append(int(port, 16))
    return sorted(ports)

def project_posix(phase, pid):
    audit = subprocess.run(["python3", str(Path(__file__).with_name("test-drive-project-posix-native.py")),
        "--mount", str(mount), "--qa-pid", str(pid), "--output", str(root / ("project-posix-" + phase + ".json")),
        "--phase", phase, "--state", str(root / "project-posix-state.json")], capture_output=True, text=True)
    (root / ("project-posix-" + phase + ".log")).write_text(audit.stdout + audit.stderr)
    print(audit.stdout + audit.stderr, flush=True)
    audit.check_returncode()


def worker():
    app = None
    foreign_listener = None
    try:
        with (root / "application.log").open("w") as log:
            if args.listener_audit:
                foreign_listener = socket.socket()
                try:
                    foreign_listener.bind(("127.0.0.1", 14201))
                except OSError as error:
                    foreign_listener.close()
                    foreign_listener = None
                    if error.errno != errno.EADDRINUSE:
                        raise
                else:
                    foreign_listener.listen()
                checks["preferred stream port is occupied during native startup"] = True
            app = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
            window = wait_for("main window opens", lambda: window_for(app.pid), 45)
            mount_id = wait_for("real Drive mounts", mounted, 30)
            os.statvfs(mount)
            if args.listener_audit:
                ports = wait_for("native streaming uses an owned IPv4 listener", lambda: owned_ipv4_listeners(app.pid))
                assert len(ports) == 1 and ports[0] != 14201, ports
                result["native_stream_endpoint"] = {"host": "127.0.0.1", "port": ports[0], "occupied_preferred_port": 14201}
                checks["occupied port cannot redirect this native session to another listener"] = True
            if args.storage_audit:
                qa_db = profile / "data/io.terarelay.desktop/shares.db"
                def fixture_ready():
                    with sqlite3.connect(qa_db) as db:
                        return db.execute("SELECT count(*) FROM logical_files WHERE logical_channel_id='qa-feature-a-movies'").fetchone()[0] == 4
                wait_for("QA channel metadata is seeded", fixture_ready, 45)
                # The channel QA seed intentionally does not add mounted Drive entries.
                # Project only these synthetic objects into this newly-created private profile.
                with sqlite3.connect(qa_db) as db:
                    now = int(time.time() * 1000)
                    db.executemany("INSERT OR IGNORE INTO drive_directories (id,parent_id,name,backing_logical_channel_id,created_at,updated_at) VALUES (?,NULL,?,?,?,?)",
                        [("channel-qa-feature-a-movies", "Movies QA", "qa-feature-a-movies", now, now),
                         ("channel-qa-feature-a-archive", "Archive QA", "qa-feature-a-archive", now, now)])
                    db.execute("INSERT INTO logical_files (file_id,logical_channel_id,first_message_id,display_name,total_size,mime_type,chunk_count,storage_version,created_at,status) VALUES ('55555555555555555555555555555555','qa-feature-a-movies',91005,'10TB metadata fixture.bin',10000000000000,'application/octet-stream',1,1,?,'complete')", (int(time.time()),))
                    db.execute("INSERT OR IGNORE INTO drive_file_entries (entry_id,file_id,directory_id,display_name,created_at,updated_at) SELECT 'import-'||file_id,file_id,'channel-qa-feature-a-movies',display_name,?,? FROM logical_files WHERE logical_channel_id='qa-feature-a-movies'", (now, now))
                wait_for("QA files appear in mounted Drive", lambda: (mount / "Movies QA/Interstellar.1080p.BluRay.mkv").is_file(), 15)
            initial_entries = os.listdir(mount)
            if (args.accessibility_audit or args.settings_accessibility_audit
                    or args.settings_tab_cycle_audit or args.settings_tab_cycle_xdotool_audit
                    or args.join_channel_accessibility_audit
                    or args.remote_upload_accessibility_audit
                    or args.channel_info_accessibility_audit
                    or args.version_stack_accessibility_audit
                    or args.version_stack_tab_cycle_xdotool_audit):
                atspi_args = [
                    "python3", str(Path(__file__).with_name("test-native-atspi-qa.py")),
                    "--qa-pid", str(app.pid),
                    "--output", str(root / "native-accessibility-result.json"),
                ]
                if (args.settings_accessibility_audit or args.settings_tab_cycle_audit
                        or args.settings_tab_cycle_xdotool_audit):
                    atspi_args.append("--all-settings-tabs")
                if args.settings_tab_cycle_audit or args.settings_tab_cycle_xdotool_audit:
                    atspi_args.append("--tab-cycle-audit")
                if args.settings_tab_cycle_xdotool_audit:
                    atspi_args.append("--xdotool-keyboard")
                if args.join_channel_accessibility_audit:
                    atspi_args.append("--join-channel-dialog")
                if args.remote_upload_accessibility_audit:
                    atspi_args.append("--remote-upload-dialog")
                if args.channel_info_accessibility_audit:
                    atspi_args.append("--channel-info-dialog")
                if args.version_stack_accessibility_audit or args.version_stack_tab_cycle_xdotool_audit:
                    atspi_args.append("--version-stack-dialog")
                if args.version_stack_tab_cycle_xdotool_audit:
                    atspi_args.append("--version-stack-tab-cycle-xdotool")
                probe = subprocess.run(
                    atspi_args,
                    capture_output=True, text=True, timeout=65,
                )
                (root / "native-accessibility.log").write_text(probe.stdout + probe.stderr)
                print(probe.stdout + probe.stderr, flush=True)
                probe.check_returncode()
                checks["native AT-SPI interactive controls have accessible names"] = True
                if (args.settings_accessibility_audit or args.settings_tab_cycle_audit
                        or args.settings_tab_cycle_xdotool_audit):
                    checks["native Settings dialog has name, controls and working Close action"] = True
                if args.settings_tab_cycle_audit or args.settings_tab_cycle_xdotool_audit:
                    checks["native Settings Shift-Tab/Tab wraps within dialog"] = True
                if args.join_channel_accessibility_audit:
                    checks["native Join Channel dialog and invite field are accessible"] = True
                if args.remote_upload_accessibility_audit:
                    checks["native Remote Upload dialog and links field are accessible"] = True
                if args.channel_info_accessibility_audit:
                    checks["native Channel Info dialog and Close/focus return are accessible"] = True
            if args.storage_audit:
                subprocess.run(["python3", str(Path(__file__).with_name("test-drive-storage-read-native.py")), "--mount", str(mount), "--qa-pid", str(app.pid), "--output", str(root / "storage-read-result.json")], check=True)
                checks["storage and on-demand read audit"] = True
            if args.filesystem_audit:
                assert args.storage_audit, "Filesystem audit requires the owned Drive fixture"
                subprocess.run(["python3", str(Path(__file__).with_name("test-linux-drive-filesystem.py")), "--mount", str(mount), "--qa-pid", str(app.pid)], check=True)
                checks["real FUSE file-manager operations"] = True
            if args.project_audit:
                assert args.storage_audit, "Project audit requires the owned Drive fixture"
                project_test = subprocess.run(["python3", str(Path(__file__).with_name("test-drive-project-workflow-native.py")), "--mount", str(mount), "--qa-pid", str(app.pid), "--output", str(root / "project-capabilities.json")], capture_output=True, text=True)
                (root / "project-capabilities.log").write_text(project_test.stdout + project_test.stderr)
                project_test.check_returncode()
                checks["project capability audit executed; individual support reported separately"] = True
            if args.project_posix_audit:
                assert args.storage_audit, "Project POSIX audit requires the owned Drive fixture"
                project_posix("prepare", app.pid)
                checks["strict mounted project permissions, symlinks and npm workflow"] = True
            if args.performance_audit:
                assert args.storage_audit, "Performance audit requires the owned Drive fixture"
                performance = subprocess.run(["python3", str(Path(__file__).with_name("test-drive-metadata-performance-native.py")), "--mount", str(mount), "--qa-pid", str(app.pid), "--output", str(root / "metadata-performance-result.json")], capture_output=True, text=True)
                (root / "metadata-performance.log").write_text(performance.stdout + performance.stderr)
                print(performance.stdout + performance.stderr, flush=True)
                performance.check_returncode()
                checks["large-folder metadata scales without whole-file acquisition"] = True
            if args.recovery_audit:
                assert args.storage_audit, "Recovery audit requires the owned Drive fixture"
                subprocess.run(["fusermount3", "-u", str(mount)], check=True)
                mount_id = wait_for("detached Drive remounts while window is open", lambda: mounted(), 30)
                assert (mount / "Movies QA/10TB metadata fixture.bin").stat().st_size == 10_000_000_000_000
                checks["recovery preserves namespace and large logical sizes"] = True
            if args.cache_policy:
                wait_for("idle maintenance removes only retired file cache", lambda: not retired.exists(), 40)
                assert all(path.read_bytes() == content for path, content in protected.items())
                assert (blocks / "active.tmp").read_bytes() == b"pending cache write"
                cached = sum(p.stat().st_size for p in blocks.glob("*.bin"))
                assert 0 < cached <= 32 * 1024 * 1024 + 4096
                checks["idle maintenance bounds committed read blocks and keeps pending writes"] = True
                checks["idle maintenance preserves authorization and unavailable TDLib cache"] = True
                with sqlite3.connect(data / "shares.db") as db:
                    db.execute("UPDATE transfer_queues SET items=? WHERE kind='download'",
                               (json.dumps([{"status":"paused"}]),))
                retired.mkdir()
                resume = retired / "cached-for-resume.bin"
                resume.write_bytes(b"unfinished transfer")
                pause_started = time.monotonic()
                wait_for("another maintenance interval passes with a paused queue",
                         lambda: time.monotonic() - pause_started >= 65, 70)
                assert resume.read_bytes() == b"unfinished transfer"
                assert all(path.read_bytes() == content for path, content in protected.items())
                checks["periodic idle cleanup preserves paused transfer data"] = True
            subprocess.run(["wmctrl", "-ic", window], check=True)
            time.sleep(.5)
            assert app.poll() is None, "Closing the window exited TeraRelay and disconnected Drive"
            checks["close keeps native process alive"] = True
            wait_for("window hides", lambda: not mapped(window))
            assert mounted() == mount_id, "Window close unmounted or replaced Drive"
            os.statvfs(mount)
            assert os.listdir(mount) == initial_entries, f"Namespace changed while hidden: {initial_entries} -> {os.listdir(mount)}"
            checks["Drive remains readable while hidden"] = True
            if args.native_recovery_audit:
                # Stop only this owned QA application's WebKit web renderer.
                # Native mount recovery must survive frontend timers being unavailable.
                children = set()
                pending = [app.pid]
                while pending:
                    pid = pending.pop()
                    for task in Path(f"/proc/{pid}/task").glob("*"):
                        try:
                            found = [int(value) for value in (task / "children").read_text().split()]
                        except FileNotFoundError:
                            continue
                        for child_pid in found:
                            if child_pid not in children:
                                children.add(child_pid)
                                pending.append(child_pid)
                renderers = []
                for pid in children:
                    try:
                        if Path(f"/proc/{pid}/comm").read_text().strip().startswith("WebKitWebProces"):
                            renderers.append(pid)
                    except FileNotFoundError:
                        pass
                assert renderers, "Owned WebKit web renderer was not found"
                try:
                    for pid in renderers: os.kill(pid, signal.SIGSTOP)
                    subprocess.run(["fusermount3", "-u", str(mount)], check=True)
                    mount_id = wait_for("native recovery works while the web renderer is stopped", mounted, 20)
                    assert (mount / "Movies QA/10TB metadata fixture.bin").stat().st_size == 10_000_000_000_000
                finally:
                    for pid in renderers:
                        try: os.kill(pid, signal.SIGCONT)
                        except ProcessLookupError: pass
            if args.recovery_audit:
                subprocess.run(["fusermount3", "-u", str(mount)], check=True)
                mount_id = wait_for("detached Drive remounts while window stays hidden", lambda: mounted(), 30)
                assert not mapped(window), "Drive recovery unexpectedly opened the window"
                assert (mount / "Movies QA/10TB metadata fixture.bin").stat().st_size == 10_000_000_000_000
                checks["hidden recovery preserves Drive namespace"] = True
            wait_for("tray registers with desktop host", lambda: items)
            activate("Open TeraRelay")
            wait_for("tray Open restores same window", lambda: mapped(window))
            for number in range(3):
                subprocess.run(["wmctrl", "-ic", window], check=True)
                wait_for("hide cycle " + str(number), lambda: not mapped(window))
                assert app.poll() is None and mounted() == mount_id
                activate("Open TeraRelay")
                wait_for("reopen cycle " + str(number), lambda: mapped(window))
            checks["repeated close never remounts or disconnects Drive"] = True
            if args.crash_audit or args.project_posix_audit:
                assert args.storage_audit, "Crash audit requires the owned Drive fixture"
                draft = mount / "Archive QA/crash-recovery-owned.bin"
                payload = bytes(range(256)) * 256
                stream = draft.open("w+b", buffering=0)
                assert stream.write(payload) == len(payload)
                os.fsync(stream.fileno())
                try:
                    app.send_signal(signal.SIGSTOP)
                    time.sleep(.25)
                    app.send_signal(signal.SIGCONT)
                    assert os.pread(stream.fileno(), len(payload), 0) == payload
                    checks["process stop and resume preserves pending bytes"] = True
                    old_items = len(items)
                    old_mount_id = mounted()
                    app.kill()
                    app.wait(timeout=10)
                finally:
                    try:
                        stream.close()
                    except OSError as error:
                        # The killed FUSE daemon cannot process release on its old handle.
                        # Accept only this expected disconnect after confirmed process exit.
                        if app.poll() is None or error.errno != errno.ENOTCONN:
                            raise
                app = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
                window = wait_for("crash restart creates a new window", lambda: window_for(app.pid), 30)
                def healthy_crash_mount():
                    identity = mounted()
                    diagnostics = {"old_mount_id": old_mount_id, "mount_id": identity, "process_exit": app.poll()}
                    result["crash_recovery_diagnostics"] = diagnostics
                    # Linux can reuse a mount ID after detach; prove recovery by
                    # serving the persisted file from the new process, not ID inequality.
                    if not identity:
                        return None
                    try:
                        diagnostics["draft_size"] = draft.stat().st_size
                        return identity if diagnostics["draft_size"] == len(payload) else None
                    except OSError as error:
                        diagnostics["draft_errno"] = error.errno
                        return None
                mount_id = wait_for("crash restart replaces stale mount and retains pending file", healthy_crash_mount, 30)
                assert draft.read_bytes() == payload
                checks["pending write survives native process crash byte-for-byte"] = True
                with draft.open("r+b", buffering=0) as resumed:
                    resumed.seek(len(payload))
                    assert resumed.write(b"resumed") == 7
                    os.fsync(resumed.fileno())
                assert draft.read_bytes() == payload + b"resumed"
                checks["recovered pending write accepts a safe continuation"] = True
                draft.unlink()
                wait_for("crash fixture removed", lambda: not draft.exists())
                wait_for("crash restart registers a fresh tray", lambda: len(items) > old_items)
                if args.project_posix_audit:
                    project_posix("verify", app.pid)
                    checks["project modes, links and npm workflow survive application crash/restart"] = True
            subprocess.run(["wmctrl", "-ic", window], check=True)
            wait_for("hidden before launcher reopen", lambda: not mapped(window))
            second = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
            try:
                assert second.wait(timeout=10) == 0, "Second launcher should return to existing instance"
            finally:
                if second.poll() is None: second.terminate(); second.wait(timeout=5)
            wait_for("launcher restores same background instance", lambda: mapped(window))
            assert app.poll() is None and mounted()
            subprocess.run(["wmctrl", "-ic", window], check=True)
            wait_for("hidden before explicit Quit", lambda: not mapped(window))
            activate("Quit TeraRelay")
            assert app.wait(timeout=20) == 0, "Tray Quit failed to exit cleanly"
            wait_for("explicit Quit unmounts Drive", lambda: not mounted())
            previous_items = len(items)
            app = subprocess.Popen([binary], env=env, stdout=log, stderr=subprocess.STDOUT)
            window = wait_for("fresh launch creates window", lambda: window_for(app.pid), 30)
            wait_for("fresh launch is visible after hidden Quit", lambda: mapped(window))
            wait_for("Drive mounts on fresh launch", mounted)
            wait_for("fresh launch registers new tray", lambda: len(items) > previous_items)
            activate("Quit TeraRelay")
            assert app.wait(timeout=20) == 0
            wait_for("final Quit cleans mount", lambda: not mounted())
            result.update(pass_result=True, checks=checks)
    except Exception as error:
        result.update(pass_result=False, checks=checks, error=str(error), traceback=traceback.format_exc())
    finally:
        if app is not None and app.poll() is None:
            app.terminate()
            try: app.wait(timeout=15)
            except subprocess.TimeoutExpired: app.kill(); app.wait(timeout=5)
        if mounted():
            subprocess.run(["fusermount3", "-uz", str(mount)], check=True)
        if foreign_listener is not None:
            foreign_listener.close()
        (root / "result.json").write_text(json.dumps(result, indent=2))
        print(json.dumps(result, indent=2), flush=True)
        GLib.idle_add(loop.quit)

threading.Thread(target=worker, daemon=True).start()
loop.run()
raise SystemExit(0 if result.get("pass_result") else 1)
