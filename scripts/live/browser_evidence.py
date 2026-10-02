"""CLI polling and evidence integrity checks shared by the browser live matrices."""
import hashlib
import json
from pathlib import Path
import subprocess


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
