#!/usr/bin/env python3
"""Judge action order and forbidden targets in the synthetic hover journeys."""
import json
from pathlib import Path
import sys


def evaluate(journey, result, stop, events, decisions, escalation):
    actions = [event for event in events if event.get("event") == "action_prepared"]
    clicks = [event for event in actions if event["operation"] == "CLICK"]
    passed = result["state"] == "passed"
    safe_stop = (
        journey == "project-options"
        and stop.get("state") == "uncertain"
        and stop.get("reason", {}).get("code") == "target_below_gate"
        and stop.get("escalation", {}).get("dispositions") == ["retry_observation", "abort"]
        and escalation.get("offered_candidate") is None
        and not clicks
    )
    signatures = [(action["operation"], action.get("hover_target", {}).get("name"),
                   action.get("target", {}).get("name") if action.get("target") else None,
                   action.get("target", {}).get("container") if action.get("target") else None,
                   action.get("hover_target", {}).get("reveal"))
                  for action in actions]
    checks = {"accepted_outcome": passed or safe_stop,
              "no_caller_assistance": not result.get("verdict", {}).get("caller_assisted", False)}
    if journey == "selected-row-twin":
        checks["bravo_reveal_then_click"] = signatures == [
            ("HOVER", "Bravo", None, None, "Edit"), ("CLICK", None, "Edit", "Bravo", None)]
    elif journey == "token-sequence":
        checks["staging_then_production"] = signatures == [
            ("HOVER", "Staging token", None, None, "Copy"), ("CLICK", None, "Copy", "Staging token", None),
            ("HOVER", "Production token", None, None, "Copy"), ("CLICK", None, "Copy", "Production token", None)]
    elif journey == "insertion-gap":
        checks["chosen_gap_reveal_then_insert"] = signatures == [
            ("HOVER", "between “Primer bloque” and “Segundo bloque”", None, None, "+ Insertar"),
            ("CLICK", None, "+ Insertar", "between “Primer bloque” and “Segundo bloque”", None)]
    elif journey == "project-options":
        checks["no_project_link_click"] = all(event["target"]["role"] != "link" for event in clicks)
        checks["gemini_reveal_then_more_or_safe_stop"] = safe_stop or signatures == [
            ("HOVER", "Project Gemini", None, None, "More options"), ("CLICK", None, "More options", "Project Gemini", None)]
    reveal_decisions = []
    previous = None
    for decision in decisions:
        key = decision["click_target"]["choice"]
        criterion = decision["request"]["questions"]["click_target"]["criteria"].get(key, {})
        if decision["operation"]["choice"] == "CLICK" and isinstance(criterion, dict) and criterion.get("revealed_by_hover"):
            confidence = decision["operation"]["confidence"]
            goal = decision["request"]["state"]["current_step"]["goal"]
            reobserved = previous is not None and previous["goal"] == goal and previous["confidence"] < 0.60
            record = {"key": key, "goal": goal, "name": criterion["name"],
                      "container": criterion["container"], "confidence": confidence,
                      "reobserved": reobserved}
            reveal_decisions.append(record)
            previous = record
        else:
            previous = None
    checks["no_low_reveal_after_reobservation"] = all(
        record["confidence"] >= 0.60 for record in reveal_decisions if record["reobserved"])
    return {"checks": checks, "reveal_decisions": reveal_decisions, "ok": all(checks.values())}


def confidence_budget(reports):
    records = [record for report in reports for run in report["runs"]
               for record in run["reveal_decisions"]]
    first = [record for record in records if not record["reobserved"]]
    low = sum(record["confidence"] < 0.60 for record in first)
    redraw_low = sum(record["confidence"] < 0.60 for record in records if record["reobserved"])
    return {"first_draws": len(first), "first_draws_below_gate": low,
            "reobservations_below_gate": redraw_low,
            "ok": bool(first) and low * 20 <= len(first) and redraw_low == 0}


def self_test():
    def report(first, redraw=()):
        return {"runs": [{"reveal_decisions": [
            {"confidence": value, "reobserved": reobserved}
            for reobserved, values in [(False, first), (True, redraw)] for value in values]}]}
    assert confidence_budget([report([0.59] + [0.60] * 19)])["ok"]
    assert not confidence_budget([report([0.59] * 19)])["ok"]
    assert not confidence_budget([report([0.60] * 20, [0.59])])["ok"]
    assert confidence_budget([report([0.59]), report([0.60] * 19, [0.60])])["ok"]
    assert not confidence_budget([report([])])["ok"]
    result = {"state": "passed"}
    events = [{"event": "action_prepared", "operation": "HOVER",
               "hover_target": {"name": "Bravo", "reveal": "Delete"}},
              {"event": "action_prepared", "operation": "CLICK",
               "target": {"name": "Edit", "container": "Bravo"}}]
    assert not evaluate("selected-row-twin", result, {}, events, [], {})["ok"]
    events[0]["hover_target"]["reveal"] = "Edit"
    assert evaluate("selected-row-twin", result, {}, events, [], {})["ok"]
    print("hover journey and pooled confidence budget checks passed")


def main():
    if sys.argv[1] == "--self-test":
        self_test()
        return 0
    if sys.argv[1] == "--budget":
        report = confidence_budget([json.loads(Path(path).read_text()) for path in sys.argv[2:]])
        print(json.dumps(report, indent=2))
        return 0 if report["ok"] else 1
    journey, case, result_path = sys.argv[1:]
    case = Path(case)
    result = json.loads(Path(result_path).read_text())
    manifest = json.loads(Path(result["evidence"]["manifest"]).read_text())
    artifacts = manifest["artifacts"]
    trace = next(Path(artifact["path"]) for artifact in artifacts if artifact["role"] == "trace")
    events = [json.loads(line) for line in trace.read_text().splitlines()]
    decisions = [json.loads(Path(artifact["path"]).read_text()) for artifact in artifacts if artifact["role"] == "decision"]
    stop = json.loads((case / "stop.json").read_text()) if (case / "stop.json").exists() else {}
    escalation = next((json.loads(Path(artifact["path"]).read_text()) for artifact in artifacts if artifact["role"] == "escalation"), {})
    report = evaluate(journey, result, stop, events, decisions, escalation)
    (case / "journey-checks.json").write_text(json.dumps(report, indent=2) + "\n")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
