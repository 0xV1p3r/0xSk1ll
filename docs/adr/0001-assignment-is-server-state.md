# Assignment is live server state, not repo content

Which skills and rules are enabled in which harness on which machine is held by the server and takes effect fleet-wide as soon as it changes. It is not stored in `sk1ll.toml`, and it gets neither a draft nor a commit step. Turning a skill on or off is an operational toggle and should be immediate. Routing it through commit/push would make "enable `pdf` on the workstation" a review-and-merge chore. The repo holds only content (skills, rules, imports).

## Consequences

- We lose git history for assignments, so the server keeps an append-only assignment log with per-entry undo instead.
- `[profiles]` leaves `sk1ll.toml`, and `[targets]` becomes machine-local harness configuration (detected, with local overrides) that is reported to the server.
- Assignment uses Item IDs. A toggle takes effect when the machine has the matching item. See [ADR 0006](0006-items-have-stable-ids.md).
- Copying an Assignment between Machines carries explicit toggles for items absent on
  the destination. They take effect when those items arrive. Replace copies the source
  defaults and clears destination-only overrides, so items that inherit on the source
  inherit the same values on the destination.
