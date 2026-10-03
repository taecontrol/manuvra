#!/usr/bin/env python3
"""Foreground assertions against one disposable HTTP fixture, including live Jev and resume."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse
from urllib.request import urlopen


REPO = Path(__file__).resolve().parents[2]
os.umask(0o077)
BINARY = REPO / "target/debug/manuvra"
ROOT = REPO / ".work/live/color" / f"{time.strftime('%Y%m%d-%H%M%S')}-{os.getpid()}"
ROOT.mkdir(parents=True, mode=0o700)
REVISION = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=REPO, text=True).strip()
FIXTURE = (REPO / "tests/browser/color.html").read_text()
STATES = {}
ACK = threading.Event()
THEMES = []
ACTIVE = {}


class Fixture(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        request = urlparse(self.path)
        case = parse_qs(request.query).get("case", [""])[0]
        if request.path == "/state":
            body = json.dumps({"changed": STATES.get(case, False)}).encode()
            content_type = "application/json"
        elif request.path == "/flip":
            STATES[case] = True
            body, content_type = b"changed", "text/plain"
        elif request.path == "/ack":
            ACK.set()
            body, content_type = b"observed", "text/plain"
        elif request.path == "/theme":
            THEMES.append(case)
            body, content_type = b"recorded", "text/plain"
        else:
            # CSSOM changes emit no DOM mutation and preserve text, identity, and geometry.
            script = """
<script>
const testCase=new URL(location.href).searchParams.get('case');
const theme=document.querySelector('[aria-label="Theme"]');
theme.addEventListener('click',()=>fetch('/theme?case='+testCase));
if(testCase==='stale') {
  const sheet=document.styleSheets[0];
  const rule=sheet.cssRules[sheet.insertRule('.themed { color:#b91c1c; }',sheet.cssRules.length)];
  let changed=false;
  setInterval(async()=>{
    if(changed)return;
    const state=await (await fetch('/state?case='+testCase)).json();
    if(state.changed){changed=true;rule.style.color='#1f2937';await fetch('/ack?case='+testCase);}
  },100);
}
</script>"""
            body, content_type = (FIXTURE + script).encode(), "text/html; charset=utf-8"
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def write(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")
    path.chmod(0o600)


def command(case, label, args, provider=False):
    env = os.environ.copy()
    env["XDG_STATE_HOME"] = str(ROOT / case / "state")
    if not provider:
        env.pop("TYPESAFE_API_KEY", None)
    result = subprocess.run([str(BINARY), *args], env=env, capture_output=True, timeout=100)
    (ROOT / case / f"{label}.stdout.json").write_bytes(result.stdout)
    (ROOT / case / f"{label}.stderr.txt").write_bytes(result.stderr)
    value = json.loads(result.stdout)
    assert result.returncode in (0, 2, 3, 4, 5, 6), value
    if value.get("run_id"):
        ACTIVE[case] = value
    return value


def settle(case, value, provider=False):
    for index in range(8):
        if value["state"] != "running":
            return value
        value = command(case, f"status-{index}", ["status", value["run_id"], "--wait-ms", "30000"], provider)
    raise AssertionError(f"{case}: run did not reach a checkpoint")


def color(target, comparator="equals", expected="#b91c1c"):
    return {"color": {"target": target, comparator: expected}}


def job(case, assertions):
    return {
        "schema_version": 1, "target": {"kind": "browser", "url": f"{ORIGIN}/ready?case={case}"},
        "context": {"journey": "Synthetic foreground verification", "revision": REVISION,
                    "environment": "disposable loopback HTTP", "actor": "synthetic owner",
                    "authority": "observe colors and change the theme once in the theme job"},
        "steps": [{"id": "ready", "goal": "Observe Ready", "done_when": [{"text_visible": "Ready"}]}],
        "expectations": [{"id": "color", "assertions": assertions}],
        "options": {"allowed_origins": [ORIGIN], "active_timeout_ms": 60000,
                    "pause_timeout_ms": 120000, "lifetime_ms": 300000},
    }


def run(case, wire, provider=False):
    directory = ROOT / case
    directory.mkdir(mode=0o700)
    write(directory / "job.json", wire)
    value = command(case, "run", ["run", "--request-id", f"color-{case}-{ROOT.name}",
        "--job", str(directory / "job.json"), "--evidence", str(directory / "evidence"), "--wait-ms", "30000"], provider)
    return settle(case, value, provider)


def evidence(value):
    manifest = json.loads(Path(value["evidence"]["manifest"]).read_text())
    assert value["evidence"]["complete"] and manifest["complete"]
    artifacts = {}
    for artifact in manifest["artifacts"]:
        content = Path(artifact["path"]).read_bytes()
        assert artifact["complete"] and hashlib.sha256(content).hexdigest() == artifact["digest"]
        artifacts.setdefault(artifact["role"], []).append(content)
    return artifacts


def terminal(case, value, state, assisted=False, provider_calls=0, previous_provider_calls=0, mutations=0):
    assert value["state"] == state and value["terminal"], value
    assert value["verdict"]["caller_assisted"] == assisted, value
    assert value["cleanup"]["browser"] == "closed" and value["cleanup"]["profile"] == "removed", value
    artifacts = evidence(value)
    verification = json.loads(artifacts["verification"][0])
    traces = [json.loads(line) for line in artifacts["trace"][0].splitlines()]
    calls = len(artifacts.get("decision", [])) + int(verification["provider"] is not None) + previous_provider_calls
    assert calls == provider_calls, (case, calls)
    count = sum(entry.get("event") == "action_prepared" for entry in traces)
    assert count == mutations, (case, count)
    SUMMARY.append({"case": case, "state": state, "caller_assisted": assisted,
        "provider_calls": calls, "dispositions": len(artifacts.get("disposition", [])),
        "mutations": count,
        "manifest": value["evidence"]["manifest"], "digests_verified": True, "cleanup": value["cleanup"]})
    return verification


def abort(case, value):
    return settle(case, command(case, "abort", ["abort", value["run_id"], "--request-id", f"abort-{case}-{ROOT.name}"]))


def matrix():
    negative = {"text": "-$12.34"}
    reference = {"text": "Destructive reference"}
    for case, target, state in [("exact", negative, "passed"), ("mismatch", {"text": "$12.34"}, "failed"),
                                ("painted", {"text": "-$9.00"}, "passed")]:
        wire = job(case, [color(target)])
        if case == "painted":
            wire["values"] = {"marker": {"value": "-$9.00", "description": "classified fixture amount", "secret": True}}
        value = run(case, wire)
        check = terminal(case, value, state)["expectations"][0]["assertion_checks"][0]["color"]
        if case == "painted":
            assert check["target"]["channel"] == "painted_aria_hidden"
            assert all(b"-$9.00" not in content for files in evidence(value).values() for content in files)
    value = run("duplicates", job("duplicates", [color({"text": "$0.00"})]))
    assert value["state"] == "uncertain" and value["escalation"]["dispositions"] == ["retry_observation", "abort"], value
    terminal("duplicates", abort("duplicates", value), "aborted", assisted=True)

    wire = job("theme", [color(negative, "same_as", reference), color(negative, "different_from", {"text": "$12.34"})])
    wire["steps"].append({"id": "theme", "goal": "Click the Theme button once to switch to the dark theme.",
        "done_when": [color(negative, "equals", "#f87171")]})
    value = run("theme", wire, True)
    terminal("theme", value, "passed", provider_calls=1, mutations=1)
    assert THEMES == ["theme"], THEMES

    wire = job("stale", [color(negative)])
    # A visible, true claim with a deliberately ambiguous numeric text scope creates a
    # reproducible natural-verification pause without asking Jev to guess an invisible fact.
    wire["expectations"].append({"id": "natural", "claim": "Ready is present on the page.",
        "exact_literals": [{"literal": "0.00", "within_text": "$0.00"}]})
    value = run("stale", wire, True)
    assert value["state"] == "uncertain" and "advance" in value["escalation"]["dispositions"], value
    before = evidence(value)
    write(ROOT / "stale/paused-verification.json", json.loads(before["verification"][0]))
    assert json.loads(before["verification"][0])["provider"] is not None
    urlopen(f"{ORIGIN}/flip?case=stale", timeout=5).read()
    assert ACK.wait(5), "fixture did not confirm its CSSOM color change"
    disposition = ROOT / "stale/disposition.json"
    write(disposition, {"schema_version": 1, "escalation_id": value["escalation"]["id"],
        "disposition": {"kind": "advance", "rationale": "Ready is visibly present; the repeated amounts are zero."}})
    value = settle("stale", command("stale", "resume", ["resume", value["run_id"], "--request-id",
        f"resume-stale-{ROOT.name}", "--input", str(disposition)], True), True)
    check = terminal("stale", value, "failed", provider_calls=1, previous_provider_calls=1)["expectations"]
    assert [item["result"] for item in check] == ["not_satisfied", "not_run"]
    assert check[0]["assertion_checks"][0]["color"]["target"]["rgba"] == [31, 41, 55, 255]


if __name__ == "__main__":
    assert os.environ.get("TYPESAFE_API_KEY"), "TYPESAFE_API_KEY must be exported for live Jev judgments"
    subprocess.run(["cargo", "build", "--locked", "--bin", "manuvra"], cwd=REPO, check=True)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    ORIGIN = f"http://127.0.0.1:{server.server_port}"
    worker = threading.Thread(target=server.serve_forever, daemon=True)
    worker.start()
    SUMMARY = []
    try:
        matrix()
    finally:
        write(ROOT / "summary.json", {"revision": REVISION, "fixture_sha256": hashlib.sha256(FIXTURE.encode()).hexdigest(),
            "cases": SUMMARY, "theme_effects": THEMES})
        # Keep first divergence evidence and close any browser that is still paused.
        for case, value in list(ACTIVE.items()):
            if value.get("run_id") and not value.get("terminal"):
                try:
                    abort(case, value)
                except (AssertionError, subprocess.TimeoutExpired):
                    pass
        server.shutdown()
        server.server_close()
    key = os.environ["TYPESAFE_API_KEY"].encode()
    assert not any(key in path.read_bytes() for path in ROOT.rglob("*") if path.is_file()), "provider key leaked"
    print(f"color live evidence: {ROOT / 'summary.json'}")
