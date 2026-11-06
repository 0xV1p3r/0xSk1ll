# Assignment is live hub state, not repo content

Which skills and fragments are enabled in which harness on which machine is held by the hub and takes effect fleet-wide as soon as it changes. It is not stored in `sk1ll.toml`, and it gets neither a draft nor a promote step. Turning a skill on or off is an operational toggle and should be immediate. Routing it through commit/push would make "enable `pdf` on the workstation" a review-and-merge chore. The repo holds only content (skills, fragments, imports).

## Consequences

- We lose git history for assignments, so the hub keeps an append-only assignment log with per-entry undo instead.
- `[profiles]` leaves `sk1ll.toml`, and `[targets]` becomes machine-local harness configuration (detected, with local overrides) that is reported to the hub.
- A skill that exists only in one machine's draft can be assigned only on that machine.
