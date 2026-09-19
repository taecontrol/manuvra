# Manuvra as a Jev-driven browser flow executor

Status: Accepted
Accepted: 2026-09-19
Repository revision: `63ee4a1` (branch `guetteman/whelk`, after the workspace reset)
Lifetime: implementation input until code, tests, maintained docs, the glossary and ADRs own its live content; then retire.

## 1. Problem and scope

Manuvra is restarted as a **browser flow executor** for coding agents. A calling agent submits one job (start URL, ordered steps with done conditions, named values, final expectations). Manuvra drives a dedicated Chromium over CDP, asks TypeSafe's Jev for operation, target and value per observation, owns completion, gates and mutation guards in code, and stops with a verdict and evidence. A run that stops uncertain keeps its browser alive for a bounded time and returns an escalation the calling agent answers with a typed disposition.

Decided here: the job/result/disposition/manifest contract; the run lifecycle and process model; the evidence model; the internal owners (crates) and what is rescued from the old Chrome crate; the CLI surface the installed skill relies on.

Outside: native application targets (a `target.kind` seam is reserved, nothing else), MCP, remote hosts, application fixture launch/reset, transactional rollback, hostile-page containment, the macOS port (must not be precluded; Linux/Wayland first).

Protected behavior: no autonomous mutation below the confidence gate; the same submit is never replayed on unchanged relevant state; every write is read back; action outcomes are truthful (`observed` / `not_performed` / `uncertain`); the API key never reaches stdout, evidence, or child environments; one CLI invocation prints exactly one JSON object; large evidence is referenced by absolute path.

## 2. Grounding

- Research: `docs/research/2026-09-19-jev-browser-executor-foundations.md` (keep the tested CDP transport; HTTP contract and SDK retry policy; injected snapshot primary; `insertText` default with per-key fallback; caller value map with Jev selecting the name).
- Spike iteration 1 (branch `guetteman/jev-spike`, `spikes/jev-browser-executor/REPORT.md`): ungated execution, 0/9, duplicate persisted accounts. Every wrong decision had confidence < 0.7; every decision ≥ 0.7 was correct.
- Spike iteration 2 (`REPORT-2.md`): 0.7 gate, separate done Noul, structured done assertions in code, dialog-aware snapshot, guards: English 8/9 (one safe stop on a correct click at 0.65/0.63), 0 wrong mutations, 0 duplicates, 9.8–15.6 s per journey, ~372 ms per Jev call. Code assertions carried completion; the Noul was weak on nine true states (0.46–0.54). Spanish natural-language done strings 0/3 while operation/target/value selection was unaffected (confounded with representation).
- Arena: two independent candidates (Codex gpt-6-astra) and a cross-judge (Claude Fable 5.1) on 2026-09-19. Working files are temporary; conclusions are consolidated here.
- Judge-verified code findings the implementation must not inherit: `transport.rs` (jev-spike copy, lines 384–386) classifies a WebSocket send error as `NotSent` although partial transmission cannot be excluded; the spike's native `<select>` picks the first unselected option instead of an observed one.
- Evidence limits: one React/shadcn app (money), Chromium 152, one viewport, `jev-1.13.0`, nine-run matrices. The escalation round-trip was simulated, never executed. Thresholds 0.7 / 0.8 / 0.2 are initial policy, not calibration.
- Consumer contract: `product-validation` derives Pass/Fail/Inconclusive from evidence and never from a tool's own label; it needs revision identity, first divergence, and a separate persistence seam.

## 3. Domain meaning

Accepted terms (to be added to `CONTEXT.md`): **Job**, **Step**, **Done condition**, **Run**, **Escalation**, **Disposition**, **Verdict**, **Permit**, **Evidence**. Retained from the old vocabulary: **Manuvra**, **Target** (the Chromium page under control), **Outcome**, **Artifact**. Retired: session, actor, observer, lease, execution mode, common/raw operation, locator, element reference, reference epoch, frame token, artifact manifest as a session index, raw-usage aggregate.

Bounded assumptions: one OS user controls a run; the caller launches, resets and cleans the application under test and holds authority for its effects; v1 follows one page in one Chromium; a lost host makes its run non-resumable; jobs are written in English; UI language is uncontrolled.

## 4. Caller usage

Passing journey (values and steps mirror the spike's `jobs-2/create-account.json`):

```json
{
  "schema_version": 1,
  "target": {"kind": "browser", "url": "http://127.0.0.1:4351/"},
  "context": {"journey": "Create an account with a new USD unit", "revision": "<app fingerprint>", "environment": "money run-id x", "actor": "synthetic owner", "authority": "create validation state only in this fixture"},
  "values": {
    "account_name": {"value": "Review wallet", "description": "Name of the new account"},
    "unit_symbol": {"value": "USD", "description": "Symbol of the new currency"},
    "unit_name": {"value": "US dollar", "description": "Name of the new currency"},
    "opening_balance": {"value": "12.34", "description": "Opening balance"}
  },
  "steps": [
    {"id": "open", "goal": "Open the account creation dialog.", "done_when": [{"dialog_open": "Create account"}]},
    {"id": "name", "goal": "Fill Account name with account_name.", "requires_values": ["account_name"], "done_when": [{"field": "Account name", "equals_value": "account_name"}]},
    {"id": "currency", "goal": "Open Currency or asset.", "done_when": [{"text_visible": "+ New currency or asset"}]},
    {"id": "new-unit", "goal": "Choose + New currency or asset.", "done_when": [{"dialog_open": "New currency or asset"}]},
    {"id": "symbol", "goal": "Fill Symbol with unit_symbol.", "requires_values": ["unit_symbol"], "done_when": [{"field": "Symbol", "equals_value": "unit_symbol"}]},
    {"id": "unit-name", "goal": "Fill Unit name with unit_name.", "requires_values": ["unit_name"], "done_when": [{"field": "Unit name", "equals_value": "unit_name"}]},
    {"id": "use-unit", "goal": "Confirm with Use unit.", "done_when": [{"dialog_closed": "New currency or asset"}, {"dialog_open": "Create account"}, {"text_visible": "USD"}]},
    {"id": "balance", "goal": "Fill Opening balance with opening_balance.", "requires_values": ["opening_balance"], "done_when": [{"field": "Opening balance", "equals_value": "opening_balance"}]},
    {"id": "submit", "goal": "Submit Create account.", "done_when": [{"dialog_closed": "Create account"}, {"text_visible": "Review wallet"}, {"text_visible": "12.34"}]}
  ],
  "expectations": [
    {"id": "account", "claim": "The final page shows the Review wallet account."},
    {"id": "balance", "claim": "The Review wallet account has an opening balance of 12.34."}
  ],
  "options": {"allowed_origins": ["http://127.0.0.1:4351"]}
}
```

```bash
manuvra run --request-id acct-1 --job /tmp/job.json --evidence /tmp/evidence
```

stdout, exit 0:

```json
{
  "schema_version": 1, "request_id": "acct-1", "run_id": "r_7f3a", "state": "passed", "terminal": true, "reason": null,
  "verdict": {
    "overall": "satisfied",
    "steps": [{"id": "open", "result": "satisfied", "basis": "structured"}, "…"],
    "expectations": [
      {"id": "account", "result": "satisfied", "noul": 0.97, "numeric_checks": []},
      {"id": "balance", "result": "satisfied", "noul": 0.95, "numeric_checks": [{"literal": "12.34", "present": true}]}
    ],
    "caller_assisted": false
  },
  "evidence": {"manifest": "/tmp/evidence/r_7f3a/manifest.json", "complete": true},
  "escalation": null,
  "cleanup": {"browser": "closed", "profile": "removed", "application_state": "caller_owned"}
}
```

Escalation and resume. If the consumed operation head stays below 0.70 after one re-observation, the host dispatches nothing, keeps the browser, and returns `state: "uncertain"`, exit 2, with `escalation: {id, phase, step_id, expires_at, payload, dispositions}`. The payload file holds the step and its done condition, the done result, snapshot and screenshot paths, the decision trace path, recent actions, candidates with full probabilities and confidences, permitted mutations, and the gate reason. The caller answers:

```bash
manuvra resume r_7f3a --request-id acct-1-resume --input /tmp/resume.json
```

```json
{"schema_version": 1, "escalation_id": "e_1", "disposition": {"kind": "execute", "candidate_id": "c_1"}}
```

`execute` supplies the **operation authority** that the gate withheld (accepted decision, 2026-09-19): the host takes a fresh observation, checks done first, revalidates the candidate's target identity and semantic properties, applies budgets, the replay ledger and origin guards, and then dispatches without re-asking Jev's operation gate. It cannot substitute a different target, operation or value. The result records `caller_assisted: true` and the action's `basis: caller_authority`. Other dispositions: `advance` (attests an uncertain natural-language condition, including at final verification, recorded as `basis: caller_attestation`; never overrides a false or unknown structured assertion, a failed numeric check, a pending ambiguous mutation, or a Noul ≤ 0.20), `retry_observation`, `abort`.

Missing value. A job whose `requires_values` or `equals_value` names an absent key blocks before any browser or model call: `state: "blocked"`, `reason: {code: "missing_value", value_name: "account_name", step_id: "name"}`, exit 3. A runtime `NONE_FITS` reports `value_not_provided` with the observed field description and the known value names; it never invents a key.

Recovery: `manuvra status r_7f3a` or `status --request-id acct-1` after a lost stdout. `manuvra abort r_7f3a --request-id …`. `manuvra schema job|result|disposition|manifest`, `manuvra version`.

Exit codes: 0 passed; 2 uncertain; 3 blocked; 4 failed; 5 aborted or expired; 6 running (wait elapsed); 64 invalid input, version or request conflict; 70 internal failure without a recoverable result. JSON is authoritative.

## 5. Architecture shape

Crates and dependency direction:

```text
skills/manuvra   -> installed CLI JSON contract only
manuvra-cli      -> manuvra-flow, manuvra-contract     (binary; client, host, watchdog, process, store)
manuvra-flow     -> manuvra-chrome, manuvra-jev, manuvra-contract
manuvra-chrome   -> its own CDP/browser types
manuvra-jev      -> its own HTTP/question wire types
manuvra-contract -> serde DTOs + JSON Schema, no runtime deps
```

| Owner | Interface | Owns and hides |
|---|---|---|
| `manuvra-contract` | `Job`, `RunResult`, `Disposition`, `Manifest`, schema generation | Wire versions (integer `schema_version`, unknown input fields rejected, additive output tolerated), validation of ids/bounds/assertion forms, verdict vocabulary |
| `manuvra-cli` | `run`, `resume`, `status`, `abort`, `schema`, `version`; internal `client`, `host`, `watchdog`, `process`, `store` | Admission and request-id dedup (keyed digest of canonical input), private per-user Unix socket under `$XDG_RUNTIME_DIR/manuvra/runs/<id>/` (0700, peer-credential checked), durable records under `$XDG_STATE_HOME/manuvra/runs/<id>/`, host bootstrap through inherited pipes (never argv), readiness handshake with IPC version, wait/return `running`, lifetime enforcement |
| `manuvra-flow::run` | `Run::drive_until_stop(cancel) -> Checkpoint`, `Run::apply(disposition)` | Current-step cursor, budgets, state transitions, escalation ids; completed steps are immutable |
| `manuvra-flow::judgment` | one `judge(step, observation, history, value_names) -> Judgments` | Question construction (operation head without `STEP_DONE`, speculative per-operation target heads, one value head over caller names + `NONE_FITS`, `step_done` Noul, expectation Nouls), premises restated per head, context trimming with recorded coverage, answer validation (ids, finite probabilities, sum tolerance 0.02, argmax), model identity pinning per run |
| `manuvra-flow::policy` | `decide(context, judgments) -> Next`, `authorize(candidate, history) -> Permit` | Done-first consumption, 0.70 operation gate (initial; target/value gates at 0.70 are conservative extensions to be measured), one 300 ms re-observation, mutation budgets (`mutation_limit` default 1), replay ledger keyed by operation + target/form semantics + value name + relevant page state, origin guards, caller-authority path for `execute`. `Permit` is opaque and single-use and is minted nowhere else |
| `manuvra-flow::verification` | `check_done(condition, observation) -> satisfied / not_satisfied / unknown`, `verify(expectations, observation, judgments)` | Structured assertions (`text_visible`, `text_absent`, `field`+`nonempty`, `field`+`equals_value`, `dialog_open`, `dialog_closed`, `url_contains`, optional `scope`/`dialog`/`role`), exact case-insensitive accessible-name resolution requiring exactly one visible match (ambiguity is `unknown`, never a fuzzy pick), Noul bands ≥ 0.80 / ≤ 0.20, numeric lexeme matching (`12.34` ≠ `112.34`), coverage-aware negatives |
| `manuvra-flow::values` | `resolve(name)`, `model_view`, `readback_facts` | Only admitted strings become input; preflight of references; secret classification; equality facts without exposing raw values |
| `manuvra-flow::actions` | `perform(permit, browser, values, journal) -> ActionFact` | `action_prepared` flushed before dispatch, compound input accounting, before/after evidence, readback, no blind retry |
| `manuvra-flow::evidence` | `record`, `publish(checkpoint) -> EvidenceRef` | Redaction before persistence (values, key, screenshot masking; withhold when unverifiable), temp-write-then-rename, manifest with roles/digests/completeness, first divergence retained |
| `manuvra-chrome` | `OwnedBrowser::launch`, `observe`, `perform(prepared_input)`, `capture`, `close` | Portable discovery (`--browser`, `MANUVRA_BROWSER`, PATH candidates, platform locations), owned child in the host's process group with a Linux parent-death signal, private 0700 profile, loopback CDP with ephemeral port, injected snapshot (document identity, route/title, dialog stack, focus, indexed elements with role/name/value/states/containing dialog/operations, visible and covered text, coverage flags), act-time revalidation (same document, node identity, enabled/editable, geometry, hit test), `insertText` default with select-all and readback, per-key fallback, native `<select>` by observed option identity, combobox wait for options then a later click, screenshot fencing. The rescued transport keeps `Confirmed/Rejected/NotSent/Unknown` and the journal, with send errors reclassified `Unknown` when partial transmission cannot be excluded |
| `manuvra-jev` | `evaluate(questions, deadline) -> Evaluation` | Bearer auth from memory, `POST /v1/systemone`, SDK retry policy (2 retries, 0.5 s → 5 s, 25 % jitter, 408/429/5xx/connection/timeout, `Retry-After` honored, 10 s per attempt, 30 s per logical call clipped to run budget), sanitized errors, request ids, usage |

Process model (accepted): the CLI starts the same executable as a detached **host** plus a minimal **watchdog**. The host owns the loop, values and key in memory, the CDP connection and the Chromium child. The watchdog owns the host's process group and enforces pause and absolute deadlines even if the host hangs; it takes the publication lock only after the host is confirmed dead and writes `blocked/host_lost` or `expired/…` from the last checkpoint. Host detects watchdog death via a liveness pipe and shuts down. Abrupt death is detected by socket absence plus an obtainable exclusive run lock, never a bare PID. No run is ever restarted against a leftover browser.

Control flow:

```text
admit job (schema, references, origins) -> host+watchdog -> launch owned Chromium -> initial observation
for the current step:
  observe -> check_done (structured authoritative; else Noul bands)
    satisfied -> step boundary evidence -> next step
    unknown   -> one re-observation -> escalate
    not_satisfied -> judge() -> policy.decide (gate, budgets, ledger, origin) -> Permit
                  -> actions.perform (prepared record, live revalidation, one dispatch, readback, post-observation)
                  -> check_done again before any further mutation
end of plan -> fresh final observation -> verification (Nouls + numeric lexemes) -> publish -> close browser
uncertain at any point -> flush evidence, write escalation, hold browser until disposition or deadline
```

Defaults (versioned policy, recorded in evidence): headed Wayland, 120 s active time, 300 s per pause, 900 s absolute lifetime, 80 actions, 120 logical model calls, combobox option wait 300 ms, `status` returns immediately unless `--wait-ms`.

Evidence layout (`<evidence root>/<run_id>/`, 0700/0600, absolute paths in results, manifest roles are the contract, filenames are convention): `manifest.json`, `job.json` (normalized, redacted), `provenance.json`, `result.json`, `results/NNNN.json`, `trace.jsonl` (committed offset in manifest), `observations/o_N.{json,png}`, `decisions/d_N.json`, `steps/<id>.json`, `escalations/e_N.json`, `dispositions/request_N.json`, `verification/final.json`, `cleanup.json`.

## 6. Hidden complexity

- **Contract**: callers express intent and read facts; they never see CDP ids, node identity, sockets, PIDs, thresholds or prompt text.
- **CLI ↔ host**: detachment, discovery, dedup, concurrency of resumes, expiry and crash reconciliation are invisible; lost stdout is a `status`, not a browser reconstruction.
- **Policy**: the only place that can create a `Permit`; gate, budgets, ledger and caller authority live together, so a change of policy never touches browser mechanics or the skill.
- **Judgment**: question meaning, fan-out packing, validation and provider transport are one operation to the loop; a new model or question set changes one module plus calibration fixtures.
- **Verification**: assertion resolution, coverage, Noul bands and numeric lexing; ambiguity is `unknown`, never a convenient pick.
- **Chrome**: owned lifetime, snapshot fidelity, revalidation, input strategies and readback; Chromium quirks and the macOS port stay here.
- **Evidence**: redaction, atomic publication, completeness; missing required artifacts make `passed` impossible.

## 7. Alternatives and tradeoffs

Rejected: the shared registry-driven daemon of former ADR-0001 (exports sessions and leases the agent must not manage; update and failure coupling across runs); stateless rerun on escalation (replays mutations, cannot distinguish a lost submit response from an unsubmitted form); one long-lived interactive stdin/stdout process (ties the calling agent to a terminal handle); auto-restart against a leftover browser (no exactly-once effects); a value head per editable target as v1 default (untested; the single value head measured 27/27 and 0.995); generated missing-value names (a synthetic key is not the value's name); banning `advance` at final verification (leaves only abort or expiry for a run the caller has judged).

Accepted costs: one host, watchdog and Chromium per concurrent run; conservative atomic steps and more job text; strict field identity escalates ambiguous names the spike resolved fuzzily; incomplete observation or unverifiable redaction can prevent `passed`; a dead watchdog aborts a healthy run.

## 8. Synthesis provenance

Base: Candidate B. Grafts from A: real missing-value semantics; scrubbing the API key from Chromium's environment; `--browser` flag; per-state exit codes; per-name `redact_values`; verification-phase `advance` recorded as caller attestation; the single `judge()` operation; `within_text` scoping for numeric checks; Linux parent-death signal and lock-based death detection; crash-window tests. Judge verdict: B as base for its public-contract choices (target tag, verdict vocabulary distinct from Pass/Fail, `caller_assisted`), the tested single value head, the `Permit` device, the jev/questions split, and two verified code findings. Contradictions settled by the owner on 2026-09-19: `execute` supplies operation authority (option a); host plus watchdog (option a). Ties settled here as reversible policy: exact case-insensitive field matching; combobox wait 300 ms; five crates; immediate `status`.

## 9. Implementation contract

Preserve: every meaning in §4 and the closed set in §5 (code-owned completion, caller-only strings, one-use `Permit` and resume, no autonomous mutation below the gate, caller authority only through `execute` with revalidation, no blind submit replay, truthful outcomes, owned browser with bounded lifetime, evidence lifetime and redaction, one JSON object per invocation, `target.kind` tag, `schema_version` discipline).

Free: HTTP library; blocking versus async internals behind the transport contract; IPC framing; hash algorithm; filename padding; AX cross-check frequency; settling delays; module file splits within a crate; how the watchdog and host share an executable.

Validation obligations (union of both candidates): contract fixtures and one-JSON-object proofs; policy replay against recorded iteration-1/2 decisions; fault injection at every action boundary including WebSocket partial send and journal overflow; real cross-invocation resume on a fresh money fixture with a forced escalation; kill proofs (caller, host before intent, after intent, after dispatch, watchdog; browser closure within the bound with no further CLI call; PID-reuse safety); browser fixtures (controlled inputs, remounts, combobox, native select identity, covered dialogs, date inputs, shadow roots, frames, redirects, duplicate submit); evidence and secret proofs; the three money journeys three times each reported autonomous / assisted / stopped separately; an independent Product Validator. CRAP ≤ 8 per production function, `make fmt lint test crap`.

Re-enter design if: safe replay prevention requires the caller to manage browser primitives; a host cannot reliably outlive a CLI exit; cleanup cannot stay confined to the owned browser; essential journeys require overriding false structured checks; crash-transparent recovery becomes a requirement.

## 10. Open risks

Untested real escalation round-trip; weakly calibrated thresholds and possible over-stopping from target/value gates; semantic page fingerprints cannot prove external idempotency; custom controls with hidden submit semantics; residual CDP geometry races; user-space cleanup is not absolute under simultaneous process loss; redaction cannot prove absence of encoded pixel secrets; virtualized or canvas UI defeats coverage; model alias drift. Each is bounded by a stop policy and the validation obligations above.

ADR candidates: "run-owned background lifetime instead of a session/target daemon"; "never recover a crashed run by replay"; "resume supplies caller authority but never waives code verification".

## 11. Contract details settled after preflight (2026-09-19)

These close gaps the specification preflight found. They are contract, not implementation freedom.

- **Assertion shapes.** `{"text_visible": s, "scope"?: "viewport" | {"dialog": title}}`, `{"text_absent": s, "scope"?: …}` (absence is judged only within complete coverage of the scope), `{"field": name, "nonempty": true, "dialog"?: title, "role"?: role}`, `{"field": name, "equals_value": value_name, "dialog"?: title, "role"?: role}`, `{"dialog_open": title}`, `{"dialog_closed": title}`, `{"url_contains": s}`. Field resolution: exact case-insensitive accessible name within the scope; exactly one visible eligible match, otherwise `unknown`.
- **Expectations.** `{"id", "claim", "exact_literals"?: [{"literal": s, "within_text"?: s}]}`. Numeric literals are always extracted from the claim; `exact_literals` adds or scopes checks, never removes them.
- **Step fields.** `{"id", "goal", "done_when", "requires_values"?, "mutation_limit"?: 1..8 (default 1)}`.
- **Options callers may set.** `allowed_origins`, `active_timeout_ms` (default 120000), `pause_timeout_ms` (300000), `lifetime_ms` (900000), `max_actions` (80), `max_model_calls` (120), `viewport` ({width, height}), `redact_values` (list of value names), `debug: {"force_stop_at_step": step_id}` (forces one `uncertain` stop before the first mutation of that step; it can only add a stop, never authorize anything; recorded in evidence; absent from the skill). Not settable: thresholds, prompts, browser path (CLI `--browser` or `MANUVRA_BROWSER` only).
- **Option ranges.** `active_timeout_ms`, `pause_timeout_ms`: positive integers ≤ `lifetime_ms`; `lifetime_ms`: positive integer ≤ 3600000; `max_actions`: 1..500; `max_model_calls`: 1..1000; `viewport.width`: 320..7680, `viewport.height`: 240..4320; `allowed_origins`: nonempty list of `scheme://host[:port]` origins; `redact_values`: names that exist in `values`.
- **Time and budget outcomes.** Exhausting `active_timeout_ms`, `max_actions` or `max_model_calls` yields `blocked/budget_exhausted` (exit 3, published by the host). Exceeding `pause_timeout_ms` yields `expired/resume_deadline_elapsed` and exceeding `lifetime_ms` yields `expired/lifetime_elapsed` (exit 5, published by the host when alive, otherwise by the watchdog from the last checkpoint). No step deadline exists. `failed/done_condition_not_met` is reached when a step's `mutation_limit` is consumed and its done condition is trustworthily `not_satisfied` after the post-mutation check and one re-observation.
- **Gate policy v1.** Only the consumed operation head is gated (0.70). Target and value confidences are validated structurally, recorded in evidence and reported in the matrix; gating them is a later policy decision under a recorded policy version.
- **Crash-window records.** Host dies before any `action_prepared` for the current step: no action record, step `unresolved`, run `blocked/host_lost`. After `action_prepared`, before or after dispatch without receipt: action `uncertain`, run `blocked/host_lost`. Deadline reached with a hung host: watchdog requests shutdown, waits a 10 s grace, terminates the process group, publishes `expired/…` from the last checkpoint. Observable bound: the owned Chromium is gone within 15 s of the deadline or of confirmed host death, with no further CLI call.
- **Intermediate builds.** A feature visible in the job contract (natural-language `done_when`, nonempty `expectations`, an option) that the build does not implement is rejected at admission with `blocked/unsupported_in_this_build` naming the feature. A capability that only the live page reveals is defined by the interaction it requires, not by a role: a native `<select>` (needs SELECT by option identity), an editable autocomplete that needs type-then-wait-for-suggestions, a keyboard-dependent control, an off-screen control that needs scrolling, or an unsupported surface. A click-only ARIA combobox or listbox whose options are visible elements needs only CLICK and is supported wherever CLICK is. Such a capability is detected from the observation by the policy owner after observation and before any dispatch, and stops the run with `blocked/unsupported_in_this_build` (or `blocked/unsupported_surface` once slice 8 exists) naming the capability; restricting Jev's operation roster never substitutes a different mutation for the missing one. A build never reports `satisfied` for something it did not evaluate.
- **Secret values and the model.** Raw `secret` and `redact_values` strings never enter a provider request: the model view carries the value name, its description and code-computed facts (field nonempty, field equals value) only. Code checks use the private value in memory; exported evidence carries placeholders and facts.
- **Per-key typing selection.** `insertText` is the default for every text entry. A fill is one prepared action with suboperations: select-all, `insertText`, readback. If the readback proves the field value is unchanged from its pre-dispatch value (not merely different from the intended value), the same prepared action continues with per-key `dispatchKeyEvent` of the same value into the same revalidated field and a second readback; both suboperations are recorded. A readback that shows a changed but wrong value is `failed/write_readback_mismatch` with no second attempt. This is bounded redecision on proven non-effect, not a blind retry.
- **Completion criterion for the matrices.** Each of the nine implementers' runs and each validator run must reach completion in the same run, autonomously or through a real disposition; a run that stops and cannot be completed through an offered disposition counts as a failed run of the matrix and is reported as such, never tuned or rerun away. Autonomous, assisted and failed counts are all reported.
- **Acceptance treatment of stops.** A run that stops `uncertain` and completes through a real disposition is an assisted completion; a stop whose escalation payload names the correct candidate is a correct stop; both are reported, never relabeled as autonomous. The first failure or stop of any run on a revision stays in the report.
