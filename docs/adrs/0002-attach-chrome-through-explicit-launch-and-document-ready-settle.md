# ADR-0002: Attach Chrome through explicit dedicated launch and document-ready settle

Date: 2026-08-21
Status: Accepted

## Context

Chrome targets exist only when a loopback CDP endpoint answers. A Chrome already running without that endpoint is invisible. Manuvra did not spawn Chrome, and packaged help did not say how to make it discoverable, so an agent either invented process flags or stalled.

Spawning Chrome is not a common action. It creates a long-lived browser process, chooses a profile, and can surprise the human or mutate their daily session. `targets`, `doctor`, and `open` must stay attach-only: discovering or diagnosing a missing endpoint is not consent to start a browser.

After a mutation, Chrome also has to decide when the page is settled enough to capture post-action evidence. Waiting until in-flight network requests go idle never completes on pages that keep beacons, prefetch, or long-lived requests. The document already signals readiness with `DOMContentLoaded` or `load`. `outcome` still must not claim an application-level effect.

ADR-0001 keeps backend spawn and CDP details behind the adapter/CLI seam and the registry. It does not say which Chrome process Manuvra may start, or what “settled” means for a live document.

## Decision

The only product path that may spawn Chrome is an explicit `manuvra chrome launch`. It starts or reuses a visible instance with a dedicated user-data directory and loopback CDP. It does not open a site. It does not quit, kill, or overwrite the daily Chrome process or profile. `targets`, `doctor`, and `open` never spawn Chrome. `doctor` and the agent skill name `chrome launch` when the endpoint is refused.

After `navigate`, and after a click that starts a new main-frame document, settle when that document is ready (`DOMContentLoaded` or `load`) plus a short quiet window of page, DOM, and accessibility events. In-flight network does not block that settle and does not reset the quiet window. A click that starts no new document captures after a short watch. `observed` remains backend delivery plus captured post-state, not application success.

A dedicated profile isolates automation from the human’s cookies and cart. An explicit verb makes the new window a requested effect. Document lifecycle is the readiness signal we already receive and that busy sites can still satisfy.

## Consequences

- Agents have a recoverable path when Chrome is undiscoverable, without inventing CDP flags.
- Automation evidence and cart mutations land in the dedicated profile, not the daily session.
- `navigate` on a busy page can return `observed` once the document is ready; leftover network is not a failure.
- Chrome outlives a CLI command and a daemon restart. Stopping the daemon does not tear down that browser.
- A same-document or child-frame navigation is not treated as a following document. A load that never reaches document-ready still times out as `uncertain`.

## Alternatives considered

- **Documentation only:** the agent launches Chrome the way tests do. Rejected because the missing spawn path stayed a product defect and burned the agent deadline.
- **Spawn from `targets`, `doctor`, or `open`:** rejected because a missing endpoint is not consent to start a browser.
- **Quit and relaunch daily Chrome with remote debugging:** rejected because it hijacks the human’s session and cart.
- **Settle on network idle:** rejected because beacons and prefetch keep the network busy after the document is usable.
- **Headless dedicated Chrome as the default launch:** rejected for this path. The product opens a real window; headless remains a test fixture.
