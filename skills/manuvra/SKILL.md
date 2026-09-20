---
name: manuvra
description: "Validate a browser product journey with Manuvra when a coding task needs an evidence-backed run, persistence proof, or recoverable browser-flow check."
license: MIT
---

# Manuvra

Use Manuvra to exercise an isolated application fixture. The caller owns fixture launch, reset, persistence inspection, and cleanup; Manuvra owns the browser run and its evidence.

1. Record the exact application revision and launch a fresh fixture. Confirm that the fixture is the intended candidate through an independent application seam.
2. Write one version-1 job. Keep each step to one observable UI transition in English; opening a choice and selecting from it are separate steps. Prefer structured done conditions, give every supplied value a stable name and description, and use that name—not its literal string—in `requires_values` and `equals_value`. Reserve final expectations for user-visible end state, and scope an exact literal only when the scope and literal share one observed text container. Use only synthetic data and declare the fixture origin in `options.allowed_origins`.
3. Inspect the contract with `manuvra schema job` and author the JSON to that schema; `run` performs authoritative admission. Then run with a unique request id and a caller-owned evidence root:

   ```bash
   manuvra run --request-id "$request_id" --job "$job" --evidence "$evidence_root"
   ```

4. Parse the single JSON object from stdout. If it is `running`, continue with `manuvra status "$run_id" --wait-ms 30000`. After lost stdout, recover with `manuvra status --request-id "$request_id"`; keep the same request id only when recovering the same request.
5. When the run is `uncertain`, read its escalation payload and choose only a disposition listed in `escalation.dispositions`:
   - `execute`: select the offered candidate only after its operation, target, and value still match the intended step.
   - `advance`: attest only an allowed uncertain natural-language condition or final claim, and state the observed rationale.
   - `retry_observation`: request fresh evidence when current evidence is insufficient.
   - `abort`: stop when authority, intent, or safe continuation is absent.

   Write the selected disposition in the schema returned by `manuvra schema disposition`, then submit it with a new request id:

   ```bash
   manuvra resume "$run_id" --request-id "$resume_request_id" --input "$disposition"
   ```

   Repeat status and disposition handling until the run is terminal. A disposition is caller authority, not permission to ignore a mismatch.
6. Inspect the manifest at `evidence.manifest`. Require complete evidence, verify artifact digests, preserve the first divergence or escalation, and inspect observations, actions, verification, provenance, and cleanup relevant to the journey.
7. Query an application-owned persistence seam after completion. Record exact persisted entities and counts so duplicate or missing writes remain visible, then clean up the fixture.
8. Hand the Product Validator the application revision, binary identity, job, terminal result, manifest, first divergence, persistence facts, and cleanup evidence. Treat Manuvra's `passed` as one fact: the Product Validator independently returns Pass, Fail, or Inconclusive.

Exit codes classify the command result: `0` passed, `2` uncertain, `3` blocked, `4` failed, `5` aborted or expired, `6` running, `64` invalid input or request conflict, and `70` unrecoverable internal failure. The JSON object is authoritative.
