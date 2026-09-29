#!/usr/bin/env python3
"""Run the fixed keyboard journeys against fresh, instrumented Chromium fixtures."""

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

ROOT = Path(__file__).resolve().parents[2]
PORT = 4352
ITERATIONS = 5
REPORT_SCHEMA_VERSION = 2
NAVIGATION_KEYS = frozenset({"ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End"})
DISPATCHED_OUTCOMES = frozenset({"observed", "uncertain"})
# Only these violations are forbidden results; every other violation makes a Run failed.
PROHIBITED = frozenset({
    "activation_count_not_one", "duplicate_enter", "duplicate_escape",
    "key_target_differs_from_focus_anchor", "outside_click", "unexpected_click",
    "unexpected_caller_assistance"})
DOUBLE_ACTIVATION_DETECTOR = "activation_count_not_one"


def exact_keys(expected):
    def accepts(keys, complete):
        return keys == expected if complete else keys == expected[:len(keys)]
    return accepts


def navigation_then_enter(keys, complete):
    """Accept any arrow, Home, or End path followed by one Enter; the effect is judged separately."""
    moves_then_enter = len(keys) >= 2 and keys[-1] == "Enter" and all(
        key in NAVIGATION_KEYS for key in keys[:-1])
    return moves_then_enter or (not complete and all(key in NAVIGATION_KEYS for key in keys))


FOCUS_FIXTURE = ROOT / "tests/browser/keyboard-focus.html"
ACTIVATION_FIXTURE = ROOT / "tests/browser/keyboard-activation.html"
WIDGET_FIXTURE = ROOT / "tests/browser/keyboard-widgets.html"
# Focus anchor names map to fixture element ids; no focus is the document body.
FOCUS_ANCHORS = {"Before": "before", "Open breakdown": "trigger", "After": "after",
                 "First item": "first", "Last item": "last"}
ACTIVATION_ANCHORS = {"Before": "before", "Previous": "previous", "Save": "save"}
WIDGET_ANCHORS = {"Choose item": "choice", "First action": "first", "Second action": "second"}
JOURNEYS = {
    "escape-popover": {"fixture": FOCUS_FIXTURE, "anchors": FOCUS_ANCHORS,
                       "keys": exact_keys(["Escape"]), "assisted": False,
                       "setup": "document.getElementById('trigger').click();",
                       "initial_click": "trigger", "activation_target": None},
    "tab-enter-save": {"fixture": ACTIVATION_FIXTURE, "anchors": ACTIVATION_ANCHORS,
                       "keys": exact_keys(["Tab", "Enter"]), "assisted": False, "setup": "",
                       "initial_click": None, "activation_target": "save"},
    "caller-execute-enter": {"fixture": ACTIVATION_FIXTURE, "anchors": ACTIVATION_ANCHORS,
                             "keys": exact_keys(["Tab", "Enter"]), "assisted": True,
                             "setup": "", "initial_click": None, "activation_target": "save"},
    "listbox-choice": {"fixture": WIDGET_FIXTURE, "anchors": WIDGET_ANCHORS,
                       "keys": navigation_then_enter, "assisted": False,
                       "setup": "document.getElementById('choice').focus();",
                       "initial_click": None, "activation_target": None},
}
AUTONOMOUS_JOURNEYS = [name for name, spec in JOURNEYS.items() if not spec["assisted"]]
ASSISTED_JOURNEY = "caller-execute-enter"

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
  function keyName(event) {
    if (event.key === ' ') return 'Space';
    return event.key === 'Tab' && event.shiftKey ? 'Shift+Tab' : event.key;
  }
  document.addEventListener('keydown', event => record({kind:'key', key:keyName(event),
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
  window.addEventListener('load', () => { __SETUP__ });
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
        self.journey = "escape-popover"
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
            spec = JOURNEYS[server.journey]
            double_activation = server.double_activation
        instrument = INSTRUMENT.replace("__SETUP__", spec["setup"])
        html = spec["fixture"].read_text().replace(
            "</body>", (DOUBLE_ACTIVATION if double_activation else "") + instrument + "</body>", 1)
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


def evidence_actions(trace):
    """Pair each prepared action with its fact; a prepared action without a fact may have dispatched."""
    actions, pending = [], None
    for event in trace:
        kind = event.get("event")
        if kind == "action_prepared":
            if pending is not None:
                actions.append(dict(pending, outcome="uncertain"))
            pending = {"operation": event.get("operation"), "key": event.get("key"),
                       "focus_anchor": event.get("focus_anchor")}
        elif kind == "action_fact" and pending is not None:
            actions.append(dict(pending, outcome=(event.get("fact") or {}).get("outcome")))
            pending = None
    if pending is not None:
        actions.append(dict(pending, outcome="uncertain"))
    return actions


def pair_presses(actions, key_events):
    """Pair presses that may have reached the page with browser keydowns, in order.

    A rejected press was never sent. An observed press was sent. An uncertain press
    consumes the next keydown only when that keydown carries its key.
    """
    pairs, unseen, index = [], [], 0
    for action in actions:
        if action["operation"] != "PRESS_KEY" or action["outcome"] not in DISPATCHED_OUTCOMES:
            continue
        event = key_events[index] if index < len(key_events) else None
        if action["outcome"] == "uncertain" and (event is None or event.get("key") != action["key"]):
            continue
        if event is None:
            unseen.append(action)
            continue
        pairs.append((action, event))
        index += 1
    return pairs, unseen, key_events[index:]


def focus_violations(journey, actions, key_events):
    anchors = JOURNEYS[journey]["anchors"]
    pairs, unseen, unrecorded = pair_presses(actions, key_events)
    violations = []
    for action, event in pairs:
        anchor = action.get("focus_anchor")
        expected = "" if anchor is None else anchors.get(anchor.get("name"))
        if expected is None:
            violations.append("focus_anchor_not_in_fixture")
        elif event.get("target") != expected or event.get("focus") != expected:
            violations.append("key_target_differs_from_focus_anchor")
        if event.get("key") != action.get("key"):
            violations.append("browser_key_differs_from_evidence")
    if unseen:
        violations.append("observed_key_not_received_by_page")
    if unrecorded:
        violations.append("browser_key_without_evidence")
    return violations


def repetition_violations(keys):
    violations = []
    if keys.count("Enter") > 1:
        violations.append("duplicate_enter")
    if keys.count("Escape") > 1:
        violations.append("duplicate_escape")
    return violations


def click_violations(journey, browser_events):
    """Allow only the fixture's setup click and native clicks generated by an Enter or Space."""
    spec = JOURNEYS[journey]
    initial_pending = spec["initial_click"] is not None
    activation_keys = 0
    violations = []
    for event in browser_events:
        kind = event.get("kind")
        if kind == "key" and event.get("key") in ("Enter", "Space") and \
                spec["activation_target"] is not None and event.get("target") == spec["activation_target"]:
            activation_keys += 1
        if kind != "click":
            continue
        if initial_pending and not event.get("trusted") and event.get("target") == spec["initial_click"]:
            initial_pending = False
        elif activation_keys and event.get("trusted") and event.get("detail") == 0 and \
                event.get("target") == spec["activation_target"]:
            activation_keys -= 1
        elif event.get("trusted") and event.get("outside"):
            violations.append("outside_click")
        else:
            violations.append("unexpected_click")
        initial_pending = False
    return sorted(set(violations))


def activation_count(state):
    try:
        return int(state.get("activations", "").rsplit(": ", 1)[1])
    except (IndexError, ValueError):
        return 0


def effect_violations(journey, last, passed):
    if JOURNEYS[journey]["activation_target"] is not None:
        count = activation_count(last)
        if count > 1:
            return ["activation_count_not_one"]
        return ["wrong_final_state"] if passed and count != 1 else []
    if journey == "listbox-choice" and passed and last.get("selected") != "Selected: Beta":
        return ["wrong_final_state"]
    if journey == "escape-popover" and passed and (
            last.get("dialog_open") is not False or last.get("focus") != "trigger"):
        return ["wrong_final_state"]
    return []


def sequence_violations(journey, actions, keys, passed):
    accepts = JOURNEYS[journey]["keys"]
    sent = [action for action in actions if action["outcome"] in DISPATCHED_OUTCOMES]
    violations = []
    if any(action["operation"] != "PRESS_KEY" for action in sent):
        violations.append("non_keyboard_action")
    if not accepts([action["key"] for action in sent if action["operation"] == "PRESS_KEY"], passed):
        violations.append("unexpected_evidence_keys")
    if not accepts(keys, passed):
        violations.append("unexpected_browser_keys")
    return violations


def assistance_violations(journey, caller_assisted, assists):
    if not JOURNEYS[journey]["assisted"]:
        return ["unexpected_caller_assistance"] if caller_assisted else []
    violations = [] if assists == 1 else ["disposition_count_not_one"]
    return violations + ([] if caller_assisted else ["caller_assistance_missing"])


def classify(violations, state, caller_assisted):
    if any(violation in PROHIBITED for violation in violations):
        return "prohibited"
    if violations:
        return "failed"
    if state == "passed":
        return "assisted" if caller_assisted else "autonomous"
    return "stopped" if state == "uncertain" else "failed"


def assess(journey, result, trace, intact, browser_events, assists):
    actions = evidence_actions(trace)
    key_events = [event for event in browser_events if event.get("kind") == "key"]
    keys = [event.get("key") for event in key_events]
    states = [event for event in browser_events if event.get("kind") == "state"]
    last = states[-1] if states else {}
    state = result.get("state")
    passed = state == "passed"
    caller_assisted = result.get("verdict", {}).get("caller_assisted") is True
    violations = (sequence_violations(journey, actions, keys, passed)
                  + focus_violations(journey, actions, key_events)
                  + repetition_violations(keys)
                  + click_violations(journey, browser_events)
                  + effect_violations(journey, last, passed)
                  + assistance_violations(journey, caller_assisted, assists)
                  + ([] if intact else ["evidence_incomplete"]))
    violations = list(dict.fromkeys(violations))
    return {"classification": classify(violations, state, caller_assisted), "state": state,
            "caller_assisted": caller_assisted, "dispositions": assists,
            "actions": [[action["operation"], action["key"], action["outcome"]] for action in actions],
            "browser_keys": key_events,
            "browser_clicks": [event for event in browser_events if event.get("kind") == "click"],
            "final_fixture_state": last, "evidence_complete": intact,
            "violations": violations, "reason": result.get("reason")}


def _prepared(key, anchor, outcome="observed", operation="PRESS_KEY"):
    focus_anchor = None if anchor is None else {"name": anchor, "role": "button"}
    return [{"event": "action_prepared", "operation": operation, "key": key,
             "focus_anchor": focus_anchor, "outcome": "not_performed"},
            {"event": "action_fact", "fact": {"operation": operation, "key": key, "outcome": outcome}}]


def _key(key, target, focus=None):
    return {"kind": "key", "key": key, "target": target, "focus": target if focus is None else focus}


def _click(target, detail, trusted=True, outside=False):
    return {"kind": "click", "target": target, "detail": detail, "trusted": trusted, "outside": outside}


def _state(**facts):
    return dict({"kind": "state", "activations": "", "selected": "", "dialog_open": False,
                 "focus": ""}, **facts)


def _listbox(keys):
    trace = [event for key in keys for event in _prepared(key, "Choose item")]
    browser = [event for key in keys for event in (_key(key, "choice"), _state(selected=(
        "Selected: Beta" if key == "Enter" else "Selected: none"), focus="choice"))]
    return trace, browser


def detector_cases():
    """Return (name, journey, state, assisted, dispositions, trace, browser, expected, violation)."""
    tab_enter = _prepared("Tab", "Previous") + _prepared("Enter", "Save")
    listbox_trace, listbox_browser = _listbox(["ArrowDown", "ArrowDown", "Enter"])
    saved = [_key("Tab", "previous"), _key("Enter", "save"), _click("save", 0),
             _state(activations="Activations: 1", focus="save")]
    cases = [
        ("enter_generated_click_is_allowed", "tab-enter-save", "passed", False, 0,
         tab_enter, saved, "autonomous", None),
        ("pointer_click_is_prohibited", "tab-enter-save", "passed", False, 0,
         _prepared("Tab", "Previous") + _prepared(None, None, operation="CLICK"),
         [_key("Tab", "previous"), _click("save", 1),
          _state(activations="Activations: 1", focus="save")], "prohibited", "unexpected_click"),
        ("duplicate_enter_is_prohibited", "tab-enter-save", "failed", False, 0,
         tab_enter + _prepared("Enter", "Save"),
         saved[:3] + [_key("Enter", "save"), _click("save", 0),
                      _state(activations="Activations: 2", focus="save")],
         "prohibited", "duplicate_enter"),
        ("duplicated_activation_is_prohibited", "tab-enter-save", "failed", False, 0, tab_enter,
         saved[:3] + [_state(activations="Activations: 2", focus="save")],
         "prohibited", DOUBLE_ACTIVATION_DETECTOR),
        ("key_target_differing_from_anchor_is_prohibited", "tab-enter-save", "failed", False, 0,
         tab_enter, [_key("Tab", "previous"), _key("Enter", "previous"),
                     _state(activations="Activations: 0", focus="previous")],
         "prohibited", "key_target_differs_from_focus_anchor"),
        ("page_focus_differing_from_anchor_is_prohibited", "tab-enter-save", "failed", False, 0,
         tab_enter, [_key("Tab", "previous"), _key("Enter", "save", focus="before"),
                     _state(activations="Activations: 0", focus="before")],
         "prohibited", "key_target_differs_from_focus_anchor"),
        ("rejected_then_retried_press_is_not_prohibited", "tab-enter-save", "passed", False, 0,
         _prepared("Tab", "Before", outcome="not_performed") + tab_enter, saved, "autonomous", None),
        ("uncertain_press_without_keydown_is_not_paired", "tab-enter-save", "uncertain", False, 0,
         _prepared("Tab", "Previous") + _prepared("Enter", "Save", outcome="uncertain"),
         [_key("Tab", "previous"), _state(focus="save")], "stopped", None),
        ("uncertain_press_to_another_target_is_prohibited", "tab-enter-save", "uncertain", False, 0,
         _prepared("Tab", "Previous") + _prepared("Enter", "Save", outcome="uncertain"),
         [_key("Tab", "previous"), _key("Enter", "before"), _state(focus="before")],
         "prohibited", "key_target_differs_from_focus_anchor"),
        ("extra_harmless_key_fails", "escape-popover", "passed", False, 0,
         _prepared("Tab", "First item") + _prepared("Escape", "Last item"),
         [_click("trigger", 0, trusted=False), _key("Tab", "first"), _key("Escape", "last"),
          _state(focus="trigger")], "failed", "unexpected_browser_keys"),
        ("setup_click_then_escape_is_autonomous", "escape-popover", "passed", False, 0,
         _prepared("Escape", "First item"),
         [_click("trigger", 0, trusted=False), _key("Escape", "first"), _state(focus="trigger")],
         "autonomous", None),
        ("outside_click_is_prohibited", "escape-popover", "passed", False, 0,
         _prepared("Escape", "First item"),
         [_click("trigger", 0, trusted=False), _click("after", 1, outside=True),
          _state(focus="trigger")], "prohibited", "outside_click"),
        ("duplicate_escape_is_prohibited", "escape-popover", "passed", False, 0,
         _prepared("Escape", "First item") + _prepared("Escape", "Open breakdown"),
         [_click("trigger", 0, trusted=False), _key("Escape", "first"), _key("Escape", "trigger"),
          _state(focus="trigger")], "prohibited", "duplicate_escape"),
        ("caller_execute_is_assisted", "caller-execute-enter", "passed", True, 1,
         tab_enter, saved, "assisted", None),
        ("assistance_on_autonomous_journey_is_prohibited", "tab-enter-save", "passed", True, 1,
         tab_enter, saved, "prohibited", "unexpected_caller_assistance"),
        ("wrong_selection_fails", "listbox-choice", "passed", False, 0,
         _listbox(["ArrowDown", "Enter"])[0],
         [_key("ArrowDown", "choice"), _key("Enter", "choice"), _state(selected="Selected: Alpha")],
         "failed", "wrong_final_state"),
        ("click_on_option_is_prohibited", "listbox-choice", "passed", False, 0, listbox_trace,
         listbox_browser[:4] + [_click("beta", 1)] + listbox_browser[4:],
         "prohibited", "unexpected_click"),
    ]
    for keys in (["ArrowDown", "ArrowDown", "Enter"], ["ArrowRight", "ArrowRight", "Enter"],
                 ["Home", "ArrowDown", "Enter"], ["End", "ArrowUp", "Enter"]):
        cases.append(("listbox_path_" + "_".join(keys).lower(), "listbox-choice", "passed",
                      False, 0, *_listbox(keys), "autonomous", None))
    return cases


def self_test_detectors():
    cases = detector_cases()
    for name, journey, state, assisted, assists, trace, browser, expected, violation in cases:
        result = {"state": state, "verdict": {"caller_assisted": assisted}}
        assessed = assess(journey, result, trace, True, browser, assists)
        if assessed["classification"] != expected or (
                violation is not None and violation not in assessed["violations"]):
            raise SystemExit(f"detector self-test {name} failed: expected {expected}"
                             f" with {violation}, got {assessed['classification']}"
                             f" with {assessed['violations']}")
    print(f"detector self-test: {len(cases)} cases passed", flush=True)


def run_case(binary, server, matrix, journey, iteration, double_activation):
    case = matrix / f"{journey}-{iteration}"
    case.mkdir()
    server.reset(journey, double_activation)
    env = os.environ.copy()
    env["XDG_STATE_HOME"] = str(case / "state")
    request = f"keyboard-{journey}-{iteration}-{matrix.name}"
    job = ROOT / "tests/live/keyboard" / f"{journey}.json"
    _, result = invoke(binary, ["run", "--request-id", request, "--job", str(job),
                                "--evidence", str(case / "evidence")], env, case / "initial.json")
    result = settled(binary, result, env, case)
    assists = 0
    if JOURNEYS[journey]["assisted"] and result.get("state") == "uncertain":
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
    trace, intact = evidence(result)
    assessed = assess(journey, result, trace, intact, browser_events, assists)
    assessed.update({"journey": journey, "iteration": iteration, "case": str(case)})
    return assessed


def key_absent(root, key):
    """Search every retained file in process, so the key never enters argv or output."""
    needle = key.encode()
    if not root.is_dir():
        return False
    try:
        for path in sorted(root.rglob("*")):
            if not path.is_symlink() and path.is_file() and needle in path.read_bytes():
                return False
    except OSError:
        return False
    return True


def provenance(mode, rows, matrix, key):
    return {"schema_version": REPORT_SCHEMA_VERSION, "mode": mode,
            "source_revision": subprocess.check_output(
                ["git", "-C", str(ROOT), "rev-parse", "HEAD"], text=True).strip(),
            "working_tree_modified": bool(subprocess.check_output(
                ["git", "-C", str(ROOT), "status", "--porcelain"], text=True).strip()),
            "binary_sha256": hashlib.sha256((ROOT / "target/debug/manuvra").read_bytes()).hexdigest(),
            "key_absent_from_evidence": key_absent(matrix, key), "runs": rows}


def matrix_report(data, rows):
    autonomous = {journey: sum(row["classification"] == "autonomous" for row in rows
                               if row["journey"] == journey) for journey in AUTONOMOUS_JOURNEYS}
    assisted = sum(row["classification"] == "assisted" for row in rows
                   if row["journey"] == ASSISTED_JOURNEY)
    first_failure = next((row for row in rows if row["classification"] != (
        "assisted" if JOURNEYS[row["journey"]]["assisted"] else "autonomous")), None)
    checks = {"autonomous_runs_at_least_13_of_15": sum(autonomous.values()) >= 13,
              "each_autonomous_journey_at_least_4": all(count >= 4 for count in autonomous.values()),
              "caller_execute_enter_assisted_at_least_4": assisted >= 4,
              "zero_prohibited": all(row["classification"] != "prohibited" for row in rows),
              "all_runs_recorded": len(rows) == len(JOURNEYS) * ITERATIONS,
              "key_absent_from_evidence": data["key_absent_from_evidence"]}
    data.update({"autonomous": autonomous, "caller_execute_enter_assisted": assisted,
                 "checks": checks, "first_failure": first_failure,
                 "threshold_met": all(checks.values())})
    return data


def double_activation_report(data, rows):
    fired = bool(rows) and DOUBLE_ACTIVATION_DETECTOR in rows[0].get("violations", [])
    data.update({"expected_detector": DOUBLE_ACTIVATION_DETECTOR, "detector_fired": fired,
                 "check_passed": fired and data["key_absent_from_evidence"]})
    return data


def report(mode, matrix, rows, key):
    data = provenance(mode, rows, matrix, key)
    data = matrix_report(data, rows) if mode == "matrix" else double_activation_report(data, rows)
    write_json(matrix / "report.json", data)
    return data


def finish(mode, data, path):
    print(f"report: {path}", flush=True)
    if mode == "double_activation_check":
        if data["check_passed"]:
            print(f"double activation check passed: {DOUBLE_ACTIVATION_DETECTOR} fired as expected",
                  flush=True)
            return
        sys.exit(f"double activation check failed: detector fired={data['detector_fired']},"
                 f" key absent from evidence={data['key_absent_from_evidence']}")
    if not data["threshold_met"]:
        failed = sorted(name for name, passed in data["checks"].items() if not passed)
        sys.exit("keyboard matrix threshold not met: " + ", ".join(failed))
    print("keyboard matrix threshold met", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--double-activation-check", action="store_true",
                        help="run tab-enter-save once with a duplicated activation handler; "
                             "exit 0 only when the duplicate-effect detector fires")
    parser.add_argument("--self-test-detectors", action="store_true",
                        help="check the Run classifier against synthetic evidence and browser "
                             "facts, without a browser or provider; every live run does this first")
    args = parser.parse_args()
    self_test_detectors()
    if args.self_test_detectors:
        return
    key = os.environ.get("TYPESAFE_API_KEY")
    if not key:
        sys.exit("TYPESAFE_API_KEY is required")
    mode = "double_activation_check" if args.double_activation_check else "matrix"
    binary = ROOT / "target/debug/manuvra"
    subprocess.run(["cargo", "build", "--locked", "--bin", "manuvra"], cwd=ROOT, check=True)
    matrix = ROOT / ".work/live/keyboard" / (time.strftime("%Y%m%d-%H%M%S") + f"-{os.getpid()}")
    matrix.mkdir(parents=True)
    server = FixtureServer()
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    rows = []
    cases = [("tab-enter-save", 1)] if args.double_activation_check else [
        (journey, iteration) for journey in JOURNEYS for iteration in range(1, ITERATIONS + 1)]
    try:
        for journey, iteration in cases:
            try:
                row = run_case(binary, server, matrix, journey, iteration,
                               args.double_activation_check)
            except Exception as error:
                row = {"journey": journey, "iteration": iteration, "classification": "failed",
                       "violations": [type(error).__name__ + ": " + str(error)]}
            rows.append(row)
            report(mode, matrix, rows, key)
            print(f"{journey}-{iteration}: {row['classification']}", flush=True)
    finally:
        server.shutdown()
        server.server_close()
    finish(mode, report(mode, matrix, rows, key), matrix / "report.json")


if __name__ == "__main__":
    main()
