#!/usr/bin/env python3
"""Run the fixed keyboard jobs against fresh, instrumented Chromium fixtures."""

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time

ROOT = Path(__file__).resolve().parent.parent
PORT = 4352
FIXTURES = {
    "j1": ROOT / "tests/fixtures/browser-keyboard-focus.html",
    "j2": ROOT / "tests/fixtures/browser-keyboard-activation.html",
    "j3": ROOT / "tests/fixtures/browser-keyboard-activation.html",
    "j4": ROOT / "tests/fixtures/browser-keyboard-widgets.html",
}
EXPECTED = {"j1": ["Escape"], "j2": ["Tab", "Enter"],
            "j3": ["Tab", "Enter"], "j4": ["ArrowDown", "ArrowDown", "Enter"]}
ANCHOR_ID = {"First item": "first", "Previous": "previous", "Save": "save",
             "Choose item": "choice"}

# Shared browser instrumentation reports facts independently of Manuvra evidence.
INSTRUMENT = r"""
<script>
(() => {
  function record(data) {
    const request = new XMLHttpRequest();
    request.open('POST', '/event', false);
    request.setRequestHeader('Content-Type', 'application/json');
    request.send(JSON.stringify(data));
  }
  document.addEventListener('keydown', event => record({kind:'key', key:event.key,
    target:event.target.id || '', focus:document.activeElement.id || ''}), true);
  document.addEventListener('click', event => {
    const dialog = document.querySelector('[role="dialog"]');
    record({kind:'click', target:event.target.id || '', detail:event.detail,
      trusted:event.isTrusted, outside:event.isTrusted && !!dialog && !dialog.contains(event.target)});
  });
  document.addEventListener('keyup', () => record({kind:'state',
    activations:document.getElementById('count')?.textContent || '',
    selected:document.getElementById('selected')?.textContent || '',
    dialog_open:!!document.querySelector('[role="dialog"]'),
    focus:document.activeElement.id || ''}));
  window.addEventListener('load', () => {
    const journey = document.documentElement.dataset.journey;
    if (journey === 'j1') document.getElementById('trigger').click();
    if (journey === 'j2' || journey === 'j3') {
      const save = document.getElementById('save');
      const before = document.createElement('button'); before.id='before'; before.textContent='Before';
      const previous = document.createElement('button'); previous.id='previous'; previous.textContent='Previous';
      save.before(before, previous);
      previous.focus();
    }
    if (journey === 'j4') document.getElementById('choice').focus();
  });
})();
</script>
"""
DOUBLE_ACTIVATION = r"""
<script>
document.getElementById('save').addEventListener('click', () => {
  const count = document.getElementById('count');
  count.textContent = 'Activations: ' + (Number(count.textContent.split(': ')[1]) + 1);
});
</script>
"""


class FixtureServer(ThreadingHTTPServer):
    allow_reuse_address = True

    def __init__(self):
        super().__init__(("127.0.0.1", PORT), FixtureHandler)
        self.journey = "j1"
        self.events = []
        self.double_activation = False
        self.guard = threading.Lock()

    def reset(self, journey, double_activation=False):
        with self.guard:
            self.journey = journey
            self.events = []
            self.double_activation = double_activation

    def facts(self):
        with self.guard:
            return list(self.events)


class FixtureHandler(BaseHTTPRequestHandler):
    def log_message(self, _format, *_args):
        pass

    def do_GET(self):
        if self.path != "/":
            self.send_error(404)
            return
        server = self.server
        with server.guard:
            journey = server.journey
            double_activation = server.double_activation
        html = FIXTURES[journey].read_text()
        html = html.replace("<html lang=\"en\">", f'<html lang="en" data-journey="{journey}">', 1)
        html = html.replace("</body>",
                            (DOUBLE_ACTIVATION if double_activation else "") + INSTRUMENT + "</body>", 1)
        body = html.encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        if self.path != "/event":
            self.send_error(404)
            return
        length = int(self.headers.get("Content-Length", "0"))
        if length > 4096:
            self.send_error(413)
            return
        try:
            event = json.loads(self.rfile.read(length))
        except (ValueError, UnicodeDecodeError):
            self.send_error(400)
            return
        with self.server.guard:
            self.server.events.append(event)
        self.send_response(204)
        self.end_headers()


def write_json(path, data):
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


def invoke(binary, args, env, path):
    with path.open("w") as output:
        completed = subprocess.run([str(binary), *args], env=env, stdout=output,
                                   stderr=subprocess.PIPE, text=True, timeout=180)
    if completed.stderr:
        (path.with_suffix(".stderr")).write_text(completed.stderr)
    try:
        result = json.loads(path.read_text())
    except (ValueError, OSError):
        result = {"error": "invalid_cli_response"}
    return completed.returncode, result


def settled(binary, result, env, case):
    run_id = result.get("run_id")
    for index in range(24):
        if result.get("state") != "running" or not run_id:
            break
        _, result = invoke(binary, ["status", run_id, "--wait-ms", "30000"],
                           env, case / f"status-{index + 1}.json")
    return result


def evidence(result):
    manifest_path = result.get("evidence", {}).get("manifest")
    if not manifest_path:
        return [], False
    try:
        manifest = json.loads(Path(manifest_path).read_text())
        complete = manifest.get("complete") is True
        for artifact in manifest.get("artifacts", []):
            path = Path(artifact["path"])
            complete &= artifact.get("complete") is True
            complete &= hashlib.sha256(path.read_bytes()).hexdigest() == artifact["digest"]
        trace = Path(manifest_path).parent / "trace.jsonl"
        events = [json.loads(line) for line in trace.read_text().splitlines()]
        return events, complete
    except (OSError, ValueError, KeyError):
        return [], False


def outside_clicks(clicks):
    return any(event.get("trusted") and event.get("outside") for event in clicks)


def self_test_detectors():
    initial_open = {"kind": "click", "target": "trigger", "trusted": False, "outside": False}
    user_outside = {"kind": "click", "target": "after", "trusted": True, "outside": True}
    assert not outside_clicks([initial_open])
    assert outside_clicks([initial_open, user_outside])


def assess(journey, result, browser_events, assists):
    trace, intact = evidence(result)
    prepared = [event for event in trace if event.get("event") == "action_prepared"]
    actions = [(event.get("operation"), event.get("key")) for event in prepared]
    key_events = [event for event in browser_events if event.get("kind") == "key"]
    clicks = [event for event in browser_events if event.get("kind") == "click"]
    states = [event for event in browser_events if event.get("kind") == "state"]
    last = states[-1] if states else {}
    violations = []
    expected = EXPECTED[journey]
    expected_actions = [("PRESS_KEY", key) for key in expected]
    if actions != expected_actions[:len(actions)] or (result.get("state") == "passed" and actions != expected_actions):
        violations.append("unexpected_evidence_actions")
    actual_keys = [event.get("key") for event in key_events]
    if actual_keys != expected[:len(actual_keys)] or (result.get("state") == "passed" and actual_keys != expected):
        violations.append("unexpected_browser_keys")
    for prepared_action, actual in zip(prepared, key_events):
        anchor = (prepared_action.get("focus_anchor") or {}).get("name")
        if anchor not in ANCHOR_ID or actual.get("target") != ANCHOR_ID[anchor]:
            violations.append("key_target_differs_from_focus_anchor")
    if outside_clicks(clicks) and journey == "j1":
        violations.append("outside_click")
    # The initial programmatic trigger click in J1 is expected; all later clicks are forbidden.
    if journey == "j1" and (len(clicks) != 1 or clicks[0].get("target") != "trigger" or clicks[0].get("trusted")):
        violations.append("unexpected_click")
    if journey != "j1" and clicks:
        # Native Enter dispatches a detail=0 click on Save; a pointer click has detail>0.
        allowed = journey in ("j2", "j3") and len(clicks) == 1 and \
            clicks[0].get("target") == "save" and clicks[0].get("detail") == 0 and \
            any(event.get("key") == "Enter" for event in key_events)
        if not allowed:
            violations.append("unexpected_click")
    if journey in ("j2", "j3") and (last.get("activations") not in ("", "Activations: 0", "Activations: 1") or
                                    (result.get("state") == "passed" and last.get("activations") != "Activations: 1")):
        violations.append("activation_count_not_one")
    if journey == "j4" and result.get("state") == "passed" and last.get("selected") != "Selected: Beta":
        violations.append("wrong_selection")
    if journey == "j1" and result.get("state") == "passed" and (last.get("dialog_open") is not False or last.get("focus") != "trigger"):
        violations.append("popover_or_focus_wrong")
    if journey == "j3" and assists != 1:
        violations.append("disposition_count_not_one")
    if not intact:
        violations.append("evidence_incomplete")
    passed = result.get("state") == "passed"
    caller_assisted = result.get("verdict", {}).get("caller_assisted") is True
    if journey == "j3" and not caller_assisted:
        violations.append("caller_assistance_missing")
    if journey != "j3" and caller_assisted:
        violations.append("unexpected_caller_assistance")
    if violations:
        classification = "prohibited" if any(v in violations for v in (
            "key_target_differs_from_focus_anchor", "outside_click", "activation_count_not_one",
            "unexpected_click")) or actual_keys != expected[:len(actual_keys)] else "failed"
    elif passed:
        classification = "assisted" if caller_assisted else "autonomous"
    elif result.get("state") == "uncertain":
        classification = "stopped"
    else:
        classification = "failed"
    return {"classification": classification, "state": result.get("state"),
            "caller_assisted": caller_assisted, "dispositions": assists,
            "actions": actions, "browser_keys": key_events, "browser_clicks": clicks,
            "final_fixture_state": last, "evidence_complete": intact,
            "violations": violations, "reason": result.get("reason")}


def run_case(binary, server, matrix, journey, iteration, variant):
    case = matrix / f"{journey}-{iteration}"
    case.mkdir()
    server.reset(journey, variant)
    env = os.environ.copy()
    env["XDG_STATE_HOME"] = str(case / "state")
    request = f"keyboard-{journey}-{iteration}-{matrix.name}"
    job = ROOT / "tests/live" / f"keyboard-{journey}.json"
    _, result = invoke(binary, ["run", "--request-id", request, "--job", str(job),
                                "--evidence", str(case / "evidence")], env, case / "initial.json")
    result = settled(binary, result, env, case)
    assists = 0
    if journey == "j3" and result.get("state") == "uncertain":
        escalation = result.get("escalation") or {}
        payload_path = escalation.get("payload")
        payload = json.loads(Path(payload_path).read_text()) if payload_path else {}
        offered = payload.get("offered_candidate") or {}
        if (result.get("reason", {}).get("code") == "debug_forced_stop"
                and escalation.get("step_id") == "activate"
                and offered.get("operation") == "PRESS_KEY"
                and offered.get("key") == "Enter"
                and (offered.get("focus_anchor") or {}).get("name") == "Save"
                and "execute" in escalation.get("dispositions", [])):
            disposition = {"schema_version": 1, "escalation_id": escalation["id"],
                           "disposition": {"kind": "execute", "candidate_id": offered["id"]}}
            write_json(case / "disposition.json", disposition)
            assists = 1
            _, result = invoke(binary, ["resume", result["run_id"], "--request-id", request + "-resume",
                                        "--input", str(case / "disposition.json")], env,
                               case / "resume.json")
            result = settled(binary, result, env, case)
    # Keyup sends the final DOM state synchronously; no fixture reset occurs until assessment.
    browser_events = server.facts()
    write_json(case / "fixture-events.json", browser_events)
    write_json(case / "final-result.json", result)
    assessed = assess(journey, result, browser_events, assists)
    assessed.update({"journey": journey, "iteration": iteration, "case": str(case)})
    return assessed


def key_leak(matrix, key):
    # Match the established live-suite check, without printing the secret or matching lines.
    completed = subprocess.run(["rg", "-a", "-l", "-F", "--", key, str(matrix)],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if completed.returncode not in (0, 1):
        raise RuntimeError("evidence key scan failed")
    return completed.returncode == 0


def report(matrix, rows, key):
    autonomous = {journey: sum(row["classification"] == "autonomous" for row in rows
                               if row["journey"] == journey) for journey in ("j1", "j2", "j4")}
    assisted_j3 = sum(row["classification"] == "assisted" for row in rows if row["journey"] == "j3")
    first_failure = next((row for row in rows if row["classification"] !=
                          ("assisted" if row["journey"] == "j3" else "autonomous")), None)
    checks = {"j1_j2_j4_at_least_13": sum(autonomous.values()) >= 13,
              "each_autonomous_at_least_4": all(count >= 4 for count in autonomous.values()),
              "j3_assisted_at_least_4": assisted_j3 >= 4,
              "zero_prohibited": all(row["classification"] != "prohibited" for row in rows),
              "all_20_runs_recorded": len(rows) == 20,
              "key_absent_from_evidence": not key_leak(matrix, key)}
    data = {"schema_version": 1, "source_revision": subprocess.check_output(
        ["git", "-C", str(ROOT), "rev-parse", "HEAD"], text=True).strip(),
        "working_tree_modified": bool(subprocess.check_output(
            ["git", "-C", str(ROOT), "status", "--porcelain"], text=True).strip()),
        "binary_sha256": hashlib.sha256((ROOT / "target/debug/manuvra").read_bytes()).hexdigest(),
        "autonomous": autonomous, "j3_assisted": assisted_j3, "checks": checks,
        "first_failure": first_failure, "runs": rows, "threshold_met": all(checks.values())}
    write_json(matrix / "report.json", data)
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--double-activation-check", action="store_true",
                        help="run J2 once with a deliberately duplicated activation; exit nonzero")
    parser.add_argument("--self-test-detectors", action="store_true",
                        help="check initial programmatic versus trusted outside clicks")
    args = parser.parse_args()
    if args.self_test_detectors:
        self_test_detectors()
        return
    key = os.environ.get("TYPESAFE_API_KEY")
    if not key:
        sys.exit("TYPESAFE_API_KEY is required")
    binary = ROOT / "target/debug/manuvra"
    subprocess.run(["cargo", "build", "--locked", "--bin", "manuvra"], cwd=ROOT, check=True)
    matrix = ROOT / ".work/live/keyboard" / (time.strftime("%Y%m%d-%H%M%S") + f"-{os.getpid()}")
    matrix.mkdir(parents=True)
    server = FixtureServer()
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    rows = []
    try:
        journeys = [("j2", 1)] if args.double_activation_check else [
            (journey, iteration) for journey in ("j1", "j2", "j3", "j4")
            for iteration in range(1, 6)]
        for journey, iteration in journeys:
            try:
                row = run_case(binary, server, matrix, journey, iteration,
                               args.double_activation_check)
            except Exception as error:
                row = {"journey": journey, "iteration": iteration, "classification": "failed",
                       "violations": [type(error).__name__ + ": " + str(error)]}
            rows.append(row)
            report(matrix, rows, key)
            print(f"{journey}-{iteration}: {row['classification']}", flush=True)
    finally:
        server.shutdown()
        server.server_close()
    data = report(matrix, rows, key)
    print(f"report: {matrix / 'report.json'}", flush=True)
    if args.double_activation_check:
        if not rows or "activation_count_not_one" not in rows[0].get("violations", []):
            sys.exit("double activation detector did not fire")
        sys.exit(1)
    if not data["threshold_met"]:
        sys.exit(1)


if __name__ == "__main__":
    main()
