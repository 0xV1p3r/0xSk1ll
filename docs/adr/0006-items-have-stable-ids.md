# Items have stable IDs

Status: accepted. Item IDs are implemented. Assignment by Item ID lands in Phase 2.

Each personal skill, Rule and Import has an Item ID. Personal skills keep it in
`.sk1ll-id`, Rules in frontmatter, and Imports in their manifest entry. Assignment
uses the Item ID, so a rename is local to the repo and the ID moves with the item.

## Consequences

- Toggles for unknown Item IDs are inert. The server accepts toggle writes
  without checking item existence and never infers deletions from git diffs.
  This supersedes the push cleanup and toggle sweeping in
  [ADR 0005](0005-join-code-scopes.md) and removes the draft-discard protocol.
- An orphan list shows stale toggles. Only an explicit prune clears them.
- The daemon and mutating commands assign a fresh ULID to items with a
  missing ID. The write appears in the Draft. Until then, read-only callers
  see a null ID and sync uses the Harness defaults for that item.
- Item ID claims must be unique across the repo. `sk1ll repair-ids` keeps the
  first claim by path and assigns fresh IDs to the others.
- A rename carries the explicit ID with the item. Recovery
  records the placed Item ID so Adopt can find a renamed item or bring back a
  deleted one.
