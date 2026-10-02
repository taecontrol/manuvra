# Manuvra vocabulary

**Manuvra**:
The browser flow executor in this repository.

**Job**:
The calling agent's complete request: a target (kind and start URL), ordered steps, named values, final expectations, and options.
_Avoid_: task, script, test case

**Step**:
One goal with one done condition. The executor works only on the current step and never on future ones.
_Avoid_: action, command

**Done condition**:
A step's completion rule: a conjunction of structured assertions checked in code (authoritative) or a natural-language string judged by a Noul with fixed bands.
_Avoid_: assertion when the natural-language form is meant, expectation

**Run**:
One execution of a job against one owned Chromium. It has an id and one of the states `running`, `uncertain`, `passed`, `failed`, `blocked`, `aborted`, `expired`.
_Avoid_: session, execution, attempt

**Target**:
The Chromium page under Manuvra's control.
_Avoid_: tab, window, device

**Container**:
The nearest semantic or repeated item below a dialog that holds a control, excluding cells. Its label prefers `aria-labelledby`, `aria-label`, a heading, then the first painted text segment outside non-link controls (link text counts), or `between “…” and “…”` from neighboring text.

**Scroll region**:
A visible element whose content overflows vertically and can be scrolled by a user; the window is the document's root region.

**Overlay**:
The scroll routing scope: the topmost visible modal dialog or popup opened from a trigger that declares a popup.

**Hover region**:
A visible container on the target, such as a list row, holding controls hidden by opacity until the pointer is over it. Its name is the container’s label. An observation lists the controls it reveals. A hidden control can be chosen as a click target: policy hovers first, and the click is judged again on a new observation.
_Avoid_: hover area, reveal zone, hidden row

**Contested request**:
A judgment request whose observation lists hover regions or has controls sharing role, name, and dialog among the rendered controls in the document, or has a single click control with a container and no hover regions. The container remains relevant when virtualization leaves only one item rendered.

**Focus anchor**:
The observed identity of the target's active element: node, role, name, and containing dialog, or no focus when the document or body is active. A key candidate is bound to this anchor and revalidated immediately before dispatch.

**Permit**:
The single-use authorization minted only by the policy owner after done-first, the target gate for element clicks in contested requests, per-operation confidence gate, budget, replay-ledger and origin checks. No mutation is dispatched without one.
_Avoid_: lock, lease, approval

**Escalation**:
The payload a run publishes when it stops `uncertain`: current step and done result, snapshot, screenshot, recent actions, candidates with probabilities, permitted mutations, gate reason, allowed dispositions, expiry.
_Avoid_: error, prompt, question

**Disposition**:
The calling agent's typed answer to an escalation: `advance`, `execute <candidate>`, `retry_observation`, or `abort`. `execute` supplies operation authority; code still revalidates state and guards.
_Avoid_: override, command, retry

**Outcome**:
The factual disposition of an attempted operation: `observed`, `not_performed`, or `uncertain`. It does not assert that an application-level semantic effect occurred.
_Avoid_: success, result

**Verdict**:
The run's own result vocabulary: `satisfied`, `not_satisfied`, `unresolved`, `not_run` per step and expectation, plus `overall` and `caller_assisted`. Distinct from the Product Validator's Pass, Fail, Inconclusive, which are derived from evidence.
_Avoid_: pass, fail when referring to the tool's own result

**Evidence**:
Run-owned files under the caller-chosen directory: manifest, normalized job, provenance, results, trace, observations, decisions, steps, escalations, dispositions, verification, cleanup. Written incrementally and redacted before persistence.
_Avoid_: logs, output

**Artifact**:
One evidence file, addressed by absolute path and listed in the manifest with its role, digest and completeness.
_Avoid_: attachment
