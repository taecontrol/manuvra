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
                   action.get("target", {}).get("container") if action.get("target") else None)
                  for action in actions]
    checks = {"accepted_outcome": passed or safe_stop,
              "no_caller_assistance": not result.get("verdict", {}).get("caller_assisted", False)}
    if journey == "selected-row-twin":
        checks["bravo_reveal_then_click"] = signatures == [
            ("HOVER", "Bravo", None, None), ("CLICK", None, "Edit", "Bravo")]
    elif journey == "token-sequence":
        checks["staging_then_production"] = signatures == [
            ("HOVER", "Staging token", None, None), ("CLICK", None, "Copy", "Staging token"),
            ("HOVER", "Production token", None, None), ("CLICK", None, "Copy", "Production token")]
    elif journey == "insertion-gap":
        checks["chosen_gap_reveal_then_insert"] = signatures == [
            ("HOVER", "between “Primer bloque” and “Segundo bloque”", None, None),
            ("CLICK", None, "+ Insertar", "between “Primer bloque” and “Segundo bloque”")]
    elif journey == "project-options":
        checks["no_project_link_click"] = all(event["target"]["role"] != "link" for event in clicks)
        checks["gemini_reveal_then_more_or_safe_stop"] = safe_stop or signatures == [
            ("HOVER", "Project Gemini", None, None), ("CLICK", None, "More options", "Project Gemini")]
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


def main():
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
