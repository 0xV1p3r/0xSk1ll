# Items have stable IDs

Each personal skill, Rule and Import has an Item ID. Personal skills keep it in
`.sk1ll-id`, Rules in frontmatter, and Imports in their manifest entry. Assignment
uses the Item ID, so a rename is local to the repo and the ID moves with the item.

## Consequences

- Toggles for unknown Item IDs are inert. The server accepts toggle writes
  without checking item existence and never infers deletions from git diffs.
  This supersedes the push cleanup and toggle sweeping in
  [ADR 0005](0005-join-code-scopes.md) and removes the draft-discard protocol.
- An orphan list shows stale toggles. Only an explicit prune clears them.
- An item without an explicit ID derives one from its kind and name. A hand-made
  item re-created under the same name gets its old toggles back.
