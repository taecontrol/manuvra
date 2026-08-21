# ADR-0003: Bootstrap empty Chrome endpoints with one blank target

Date: 2026-08-21
Status: Accepted

## Context

`manuvra chrome launch` can reach a valid loopback Chrome debugging endpoint whose target list is empty. Chrome 151 exhibited this state even though the process was started with `about:blank`. Waiting for a page cannot recover when no other actor will create one, so launch consumes its deadline without producing a target that a session can open.

Creating a target is a mutation. A failed or delayed HTTP response does not prove that Chrome rejected it, and replaying the request can create duplicate tabs. Discovery reads can also fail transiently while Chrome publishes the new target, while malformed HTTP or JSON means the endpoint is no longer trustworthy.

ADR-0002 authorizes only explicit `chrome launch` to start or reuse the dedicated Chrome instance, but does not define recovery from a healthy endpoint with no page targets.

## Decision

When explicit `chrome launch` observes a valid Chrome endpoint with no discoverable page, it creates one `about:blank` target through Chrome's loopback HTTP debugging endpoint. It marks creation as attempted before dispatch and issues at most one creation request per invocation. The subsequent target list, not the creation response, is authoritative for launch success.

Manuvra retries transient target-list reads within the existing launch deadline. It does not retry the creation mutation, and it fails promptly when discovery returns a definitive malformed or non-Chrome response.

This recovery stays inside `chrome launch`. `targets`, `doctor`, and `open` remain attach-only, no site is opened, and the existing `launched` or `reused` result remains unchanged.

## Consequences

- A healthy but targetless Chrome endpoint becomes usable without spending the full launch timeout.
- An ambiguous creation response cannot cause duplicate tabs within one invocation.
- Recovery may visibly create one blank tab, which is part of the already explicit launch effect.
- The configured loopback endpoint remains trusted as the Chrome instance to reuse; proving endpoint ownership is a separate concern.
- Browser-level target lifecycle management is not introduced for this narrow recovery.

## Alternatives considered

- **Keep waiting for Chrome to create its startup page:** rejected because an empty endpoint can remain empty indefinitely.
- **Rely on additional Chrome process flags:** rejected because flags provide no creation acknowledgement and cannot repair an already running empty endpoint.
- **Use browser-level CDP `Target.createTarget`:** deferred because it requires browser WebSocket transport and broader lifecycle policy that this recovery does not need.
- **Retry target creation after a transport failure:** rejected because the first request may already have taken effect.
