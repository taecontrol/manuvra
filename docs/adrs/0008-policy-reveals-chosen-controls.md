# ADR-0008: Policy reveals chosen controls before judging the click

Date: 2026-09-30
Status: Accepted

## Context

A control can be hidden by hover while a same-name control in another row is visible. Asking Jev to choose between HOVER and CLICK before choosing the target loses the row distinction: the operation answer favors the visible twin even when the target answer identifies the right row. Measurements showed that Jev can choose the right hidden control when it is offered alongside visible targets with its container.

Ambiguous element choices can also favor a visible link whose name matches the intended container. Operation confidence alone does not distinguish that error. Caller execution grants operation authority under ADR-0004; it cannot supply missing confidence in the target.

## Decision

Jev chooses controls, including controls that require a hover reveal. Policy reveals a chosen hidden control, observes again, and asks Jev to judge the visible click. HOVER remains a bounded policy fallback and leaves Jev’s operation roster. A reveal never authorizes a follow-up click.

An element click in a contested request must reach target confidence 0.70 before the operation gate. A request is contested when it contains hover regions or rendered same-name controls in the document. After one re-observation, a target below the gate stops without an execute candidate. Reveal hovers keep their 0.60 operation gate and have no target gate: the measured low-confidence reveal choices were correct, and revealing does not mutate.

Containers distinguish control identity across rows for replay, focus, and revalidation. Judgment requests display containers for same-name controls and for reveal choices; uncontested requests retain their previous form. Decision evidence preserves Jev’s raw answers.

## Consequences

- A hidden control takes two ordinary policy decisions: reveal, then judge the click. The run driver needs no automatic click or additional pending state.
- Caller execution can authorize an offered reveal hover, while a failed element target gate offers only retry observation or abort.
- The threshold has a narrow measured margin. Ambiguous project links may stop safely; controls virtualized out of the document remain an existing limit.
- Container labels can change or collide. Revalidation rejects a changed label; a collision can stop replay conservatively.
- Hover detection remains limited to opacity and readable stylesheets, with the existing structural fallback. A reveal that exposes nothing stops through the replay ledger.

## Alternatives considered

- **Keep HOVER on Jev’s roster:** rejected because operation selection did not use the target’s container reliably.
- **Click automatically after revealing:** rejected because visibility, completion, and target identity need a fresh judgment.
- **Let execute bypass the target gate:** rejected because ADR-0004 grants operation authority only.
- **Gate every target or reveal:** rejected because measurements support the element CLICK gate, while correct reveal choices can have low target confidence.
