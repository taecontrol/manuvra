# ADR-0009: Policy routes untargeted scrolls

Date: 2026-10-02
Status: Accepted

## Context

A viewport can contain a popup list, modal dialog body, or in-page table whose content scrolls independently of the document. Window scrolling cannot reach their hidden controls. Controls and text outside their clipping ancestors also gave Jev false visible targets.

Clipping and criteria wording alone did not establish reliable operation selection. A prototype replayed recorded requests with controlled variations and five Jev draws per variation. For the picker, original and clipped requests produced zero of five SCROLL_DOWN choices at confidence 0.70; adding wording produced three of five. Naming the routed region produced five of five at 0.96–0.97. The dialog produced zero of five with wording alone and five of five at 0.92–0.94 with the region fact. The table reached five of five with wording and with the fact, the latter at 0.98–0.99. Controls where the target was visible chose CLICK at 0.98–1.00. Upward scrolling remained uncertain: three of five prototype choices fell below 0.70. These measurements support the fact's role, with live journeys still required to establish behavior.

## Decision

Jev chooses an untargeted scroll direction. Policy owns the destination because browser facts can settle it without another probabilistic choice. With an overlay open, scrolling stays inside the topmost overlay and never moves the document behind it. Without an overlay, a movable document wins. Otherwise, the single outermost region able to move in that direction wins; at its end, an inner region can take over. Tied regions stop before dispatch. An incomplete region inventory stops when the route depends on it.

Judgment requests name the non-document region each direction would reach, using masked page text. They omit this fact when no direction routes to a region. Clipping determines the visible choices; it does not authorize an input. Revalidation must establish the same document, connected region, remaining movement, and a wheel point that reaches that region before dispatch.

Requests that name a routed scroll region also offer `NO_CLICK_TARGET` and require the exact intended control or option. A real category picker exposed substitution of a visible option sharing only part of the requested name while the required option was still below the fold. Abstention keeps an absent target from being replaced with another item, while the operation question can choose another scroll. Existing container and hover reveal instructions remain in force. Requests with only a document route retain their shape.

A region scroll records positions of its target, scroll ancestors, and document after bounded rendering-frame stabilization. No movement is an observed outcome that consumes the replay key. An uncertain scroll re-observes with a fresh attempt key, still bounded by the existing fallback budget. Other uncertain inputs retain escalation. Document scrolling keeps its existing dispatch and evidence contract.

## Consequences

- Routing has one policy owner shared by candidates, replay, request facts, and evidence. Jev does not acquire region identity or browser-protocol authority.
- The deterministic rule gives up autonomous choice between independent tied regions. Overlay detection also excludes non-modal banners and expanded disclosures that declare no popup.
- Wheel hit testing avoids a movable inner list absorbing its parent's input. Browser chaining remains observable rather than being reconstructed as a successful action.
- Request shape affects confidence. Upward wording and the routed-region fact require live confidence and model-call budgets alongside protected hover, keyboard, and Money journeys.
- Region names can change or collide. Browser identity stays internal; persisted names are masked or redacted at their owning seams.

## Alternatives considered

- **Ask Jev to choose a region:** rejected because the accepted routing examples are deterministic and another choice would expand authority and request complexity without a demonstrated need.
- **Change only clipping and wording:** rejected by the picker and dialog measurements.
- **Repeat every unchanged scroll:** rejected because a confirmed wheel with no effect must not become an unbounded progress claim. Only uncertainty permits another attempt, within the fallback budget.
