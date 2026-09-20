---
name: manuvra
description: "Run an application journey in a dedicated Chromium with Manuvra and return evidence-backed UI validation. Use to execute a browser flow from a URL, recover or resume a Manuvra run, inspect its evidence, or prove a browser-visible change against an isolated fixture. Not for general browsing, scraping, or production data."
license: MIT
---

# Manuvra

Manuvra is a local browser-flow executor for coding agents. Give its CLI a versioned JSON **job** containing a start URL, ordered goals, caller-supplied values, completion conditions, and final expectations. It drives one dedicated Chromium, gates mutations, stops when it cannot continue safely, and returns one JSON result plus a manifest of screenshots and machine-readable evidence.

Use Manuvra against an isolated application fixture with synthetic data. You own the application process, fixture reset, authorization for its effects, persistence inspection, and cleanup. Manuvra owns only its Chromium and browser evidence. A Manuvra `passed` result proves the recorded browser journey. Combine it with an application-owned persistence check before claiming the product behavior passed.

## 1. Establish the run boundary

Confirm that the CLI and its live contract are available:

```bash
command -v manuvra
manuvra version
manuvra schema job > /tmp/manuvra-job-schema.json
manuvra schema result > /tmp/manuvra-result-schema.json
manuvra schema disposition > /tmp/manuvra-disposition-schema.json
manuvra schema manifest > /tmp/manuvra-manifest-schema.json
```

If `manuvra` is missing while working in its source checkout, install it with `cargo install --path crates/manuvra-cli --locked`. Otherwise report that the CLI is unavailable. Confirm `TYPESAFE_API_KEY` is present without printing it. Locate Chromium with `command -v chromium || command -v chromium-browser || command -v google-chrome`. You can later pass an explicit executable with `--browser`.

Launch or reset a disposable application fixture and record:

- the exact commit or build identity being exercised;
- its start URL and exact allowed origin (`scheme://host[:port]`);
- the synthetic actor and the effects this run is authorized to create;
- an application-owned query that will prove the expected persisted state and expose duplicates.

This stage is complete when the fixture is reachable, its revision is independently known, the allowed effects are bounded, and a persistence check is ready.

## 2. Author one job

Start from this shape and replace the illustrative labels and claims with the application's visible language:

```json
{
  "schema_version": 1,
  "target": {"kind": "browser", "url": "http://127.0.0.1:4351/"},
  "context": {
    "journey": "Create one synthetic account",
    "revision": "<tested commit or build id>",
    "environment": "fresh disposable fixture",
    "actor": "synthetic owner",
    "authority": "create one synthetic account in this fixture"
  },
  "values": {
    "account_name": {
      "value": "Agent validation wallet 7f3a",
      "description": "Unique name for the synthetic account"
    }
  },
  "steps": [
    {
      "id": "open",
      "goal": "Open the account creation dialog.",
      "done_when": [{"dialog_open": "Create account"}]
    },
    {
      "id": "name",
      "goal": "Fill Account name with account_name.",
      "requires_values": ["account_name"],
      "done_when": [{"field": "Account name", "equals_value": "account_name"}]
    },
    {
      "id": "submit",
      "goal": "Submit Create account.",
      "done_when": [
        {"dialog_closed": "Create account"},
        {"text_visible": "Agent validation wallet 7f3a"}
      ]
    }
  ],
  "expectations": [
    {
      "id": "account-visible",
      "claim": "The final page shows the Agent validation wallet 7f3a account."
    }
  ],
  "options": {
    "allowed_origins": ["http://127.0.0.1:4351"]
  }
}
```

Authoring rules:

- Write goals and natural-language conditions in English. Make each step one observable UI transition. Opening a chooser and selecting an option are separate steps.
- Describe intent in `goal` and the resulting state in `done_when`. A list of assertions is a conjunction. Prefer the structured forms `text_visible`, `text_absent`, `field` + `nonempty`, `field` + `equals_value`, `dialog_open`, `dialog_closed`, and `url_contains`. Use a natural-language string only when those forms cannot express the condition.
- Match field and dialog names to visible accessible labels. Manuvra requires one unambiguous visible match. Use a field's optional `dialog` or `role`, or a text assertion's `scope`, when the page repeats a label.
- Put every literal to be entered in `values` under a stable semantic name. Refer to that name from `requires_values` and `equals_value`. Keep the literal in `values`. Mark credentials or sensitive values with `"secret": true`, and list other values requiring evidence redaction in `options.redact_values`.
- Keep jobs containing classified values outside the repository in a caller-owned file with mode `0600`.
- Use unique synthetic values so persistence queries can distinguish this run. Bound browser navigation with `options.allowed_origins`.
- Reserve `expectations` for final user-visible claims. Add `exact_literals` when an exact string or number must be present, and `within_text` when it must occur in one specific observed text container.

Treat `manuvra schema job` as authoritative for optional fields, limits, and assertion shapes. Job authoring is complete when every named value reference exists, every step has an observable postcondition, final expectations describe the required visible outcome, and the context states the exact candidate and authority.

## 3. Start the run

Choose a new request id for this invocation and a caller-owned evidence root. Capture stdout as one JSON object even when the process exits nonzero:

```bash
request_id="journey-$(date +%s)-$RANDOM"
job="/absolute/path/to/job.json"
evidence_root="/absolute/path/to/evidence"
result="/tmp/$request_id-result.json"

if manuvra run \
  --request-id "$request_id" \
  --job "$job" \
  --evidence "$evidence_root" \
  --wait-ms 30000 > "$result"
then
  exit_code=0
else
  exit_code=$?
fi

jq . "$result"
run_id=$(jq -r '.run_id' "$result")
```

Add `--headless` only when a visible browser is unnecessary. Add `--browser /absolute/path/to/chromium` when discovery cannot find the intended executable.

The JSON result is authoritative. The exit code only classifies it. Starting is complete when the result is valid JSON and contains `request_id`, `run_id`, `state`, and `terminal`.

## 4. Drive the run to a terminal result

Branch on `state`:

- `running`: wait on the same run with `manuvra status "$run_id" --wait-ms 30000 > "$result"`.
- `uncertain`: inspect `escalation.payload` and the snapshot, screenshot, recent actions, and candidates it references. Choose only a kind listed in `escalation.dispositions`.
- `passed`, `failed`, `blocked`, `aborted`, or `expired`: the run is terminal. Continue to evidence validation.

After lost stdout, recover the existing run with `manuvra status --request-id "$request_id"`. Reuse a request id only to recover that exact invocation. Give every new `resume` or `abort` invocation its own request id.

For an uncertain run, create a disposition matching `manuvra schema disposition`:

- `execute`: use the exact offered `candidate_id` only when its operation, target, and value still express the intended step.
- `advance`: include a concrete `rationale` and use it only when offered for an uncertain natural-language condition or final claim that the evidence visibly establishes.
- `retry_observation`: use when fresher browser evidence can resolve the uncertainty.
- `abort`: use when authority, intent, or safe continuation is absent.

Example `execute` disposition:

```json
{
  "schema_version": 1,
  "escalation_id": "<result.escalation.id>",
  "disposition": {
    "kind": "execute",
    "candidate_id": "<candidate id from the escalation payload>"
  }
}
```

Submit it and capture the next checkpoint:

```bash
manuvra resume "$run_id" \
  --request-id "$resume_request_id" \
  --input "$disposition_file" > "$result"
```

Capture nonzero exit codes from `status` and `resume` in the same way as `run`. They classify the JSON checkpoint rather than replacing it. If authority is withdrawn before the run becomes terminal, stop it with `manuvra abort "$run_id" --request-id "$abort_request_id"`.

Repeat status and disposition handling until `terminal` is `true`. A disposition supplies bounded caller authority. Manuvra still re-observes and enforces target freshness, origin, budgets, replay guards, and code-owned verification.

## 5. Validate evidence and product state

Read the terminal JSON and the manifest at `evidence.manifest`.

For a successful Manuvra run, require all of the following:

- `state` is `passed`, `terminal` is `true`, and `verdict.overall` is `satisfied`;
- `evidence.complete` and the manifest's `complete` are `true`;
- every manifest artifact marked complete exists at its absolute path and its SHA-256 equals `digest`;
- the normalized job, provenance, observations, action trace, step facts, final verification, and cleanup agree with the result;
- any escalation and disposition remain visible, and `verdict.caller_assisted` truthfully reports assistance.

For any other terminal state, preserve `reason`, the first divergence or escalation, and the last trustworthy observation. Report the run as stopped rather than rerunning it away.

Query the application's own persistence seam after the browser run. Record the exact matching entities and counts, including zero unexpected duplicates, then clean up the fixture. The task is complete only when the report identifies the tested revision and binary, job and terminal result, manifest integrity, visible outcome, persistence facts, caller assistance, and cleanup. If a Product Validator consumes the run, give it those facts and let it independently return Pass, Fail, or Inconclusive.

Exit codes are `0` passed, `2` uncertain, `3` blocked, `4` failed, `5` aborted or expired, `6` running, `64` invalid input or request conflict, and `70` internal failure without a recoverable result.
