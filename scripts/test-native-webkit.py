#!/usr/bin/env python3
"""Run a QA page in an ephemeral native WebKitGTK view."""
import gi
import json
import sys
import warnings
import tempfile
from pathlib import Path
warnings.filterwarnings("ignore", category=DeprecationWarning)
gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
from gi.repository import Gtk, WebKit2, GLib

window = Gtk.Window()
window.set_default_size(1000, 700)
profile = tempfile.TemporaryDirectory(prefix="terarelay-webkit-qa-")
data = WebKit2.WebsiteDataManager(base_data_directory=profile.name + "/data", base_cache_directory=profile.name + "/cache")
view = WebKit2.WebView.new_with_context(WebKit2.WebContext.new_with_website_data_manager(data))
view.get_settings().set_media_playback_requires_user_gesture(False)
window.add(view)
policies = WebKit2.WebsitePolicies(autoplay=WebKit2.AutoplayPolicy.ALLOW)
def decide(view, decision, kind):
    if kind == WebKit2.PolicyDecisionType.NAVIGATION_ACTION:
        decision.use_with_policies(policies)
        return True
    return False
view.connect("decide-policy", decide)
result = {"pass": False, "error": "Native page timed out"}
pending = False

def finish(report):
    global result
    result = report
    print(json.dumps(result), flush=True)
    Gtk.main_quit()

def evaluated(view, task, *_):
    global pending, result
    pending = False
    try:
        value = view.run_javascript_finish(task).get_js_value().to_string()
        if value and value != "undefined":
            report = json.loads(value)
            result = report
            if report.get("done", True):
                finish(report)
    except Exception as error:
        finish({"pass": False, "error": str(error)})

def poll():
    global pending
    if not pending:
        pending = True
        view.run_javascript("JSON.stringify(window.__qaReport)", None, evaluated, None)
    return True

GLib.timeout_add(200, poll)
GLib.timeout_add_seconds(90, lambda: (finish({**result, "pass": False, "error": "Native page timed out"}), False)[1])
window.show_all()
if sys.argv[1].startswith(("http://", "https://")):
    view.load_uri(sys.argv[1])
else:
    view.load_html(Path(sys.argv[1]).read_text(), "http://localhost/qa")
Gtk.main()
profile.cleanup()
sys.exit(0 if result.get("pass") else 1)
