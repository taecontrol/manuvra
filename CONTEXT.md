# Manuvra

Language for a shell-driven agent that observes and controls one exact Chrome or native macOS destination while preserving honest action evidence and explicit ownership.

## Language

**Manuvra**:
The product and command that provide shell-driven computer use for coding agents. “Computer use” remains descriptive language, not the product, repository, command, or namespace name.
_Avoid_: Computer Use, computer-use, generic computer-use tool when naming the product

**Target**:
One currently discoverable Chrome or macOS automation destination selected for observation or control. Its opaque identity is interpreted only by its target adapter.
_Avoid_: Device, app, browser, destination when referring to the selected automation identity

**Target generation**:
The lifetime identity of the process, tab, or window instance behind a target. A replacement has a new generation even if user-visible names or operating-system identifiers are reused.
_Avoid_: Version, revision

**Session**:
Daemon-owned state bound to one exact target generation, role, default execution mode, and temporary evidence lifetime.
_Avoid_: Connection, context, current session when no explicit session ID exists

**Actor**:
A session role eligible to mutate its target while holding the target's actor lease.
_Avoid_: Controller, writer, owner

**Observer**:
A read-only session role that may coexist with other observers and the target's actor.
_Avoid_: Viewer, reader

**Actor lease**:
Time-limited exclusive mutation authority for one target. Exactly one session may hold it, and expiry never silently reacquires it.
_Avoid_: Lock, ownership, mutex when referring to the user-visible control contract

**Execution mode**:
The declared input-delivery policy for a session or action: `background` avoids target activation and global input; `foreground` permits visible target activation and global input.
_Avoid_: Headless, hidden, automatic mode

**Common action**:
A backend-neutral mutation with the same caller-facing guarantees across every target adapter that advertises it.
_Avoid_: Generic command, universal action

**Raw operation**:
An exact backend-specific CDP or Accessibility request whose narrower guarantees preserve capability outside the common action interface.
_Avoid_: Unsafe command, bypass

**Locator**:
Exactly one explicit element-targeting form: normalized accessible semantics, an element reference, or screenshot-frame coordinates.
_Avoid_: Selector when referring to the common interface

**Element reference**:
An opaque handle authorizing one element identity within its issuing session, target generation, and active reference epoch.
_Avoid_: Node ID, selector, reusable handle

**Reference epoch**:
The observation generation that authorizes a session's current element references. A mutation expires it immediately before possible dispatch.
_Avoid_: Tree version, snapshot ID

**Frame token**:
An opaque binding between screenshot pixels and the session, target generation, viewport or window geometry, scale, and image dimensions that produced them.
_Avoid_: Screenshot ID, coordinate space

**Action result**:
The compact factual record returned for every mutation, separating backend delivery, execution mode, post-action observation, timing, warnings, and errors.
_Avoid_: Success response, acknowledgement

**Outcome**:
The action result's factual disposition: `observed`, `not_performed`, or `uncertain`. It never asserts that an application-level semantic effect occurred.
_Avoid_: Success, passed, completed when semantic effect was not verified

**Artifact**:
A complete temporary evidence file owned by one session, addressed by absolute path and removed when the session closes unless explicitly exported.
_Avoid_: Attachment, output file

**Artifact manifest**:
The atomic session index of complete artifacts, their identities, hashes, provenance, and lifetime.
_Avoid_: Log index, evidence list

**Raw-usage aggregate**:
Optional local durable counters of backend operation name, declared intent, grouped outcome, and count used to identify candidates for promotion into common actions.
_Avoid_: Telemetry, analytics, event log
