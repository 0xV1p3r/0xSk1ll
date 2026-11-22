# Items have stable IDs

Status: accepted. Item IDs and Assignment by Item ID are implemented.

Each personal skill, Rule and Import has an Item ID. Personal skills keep it in
`.sk1ll-id`, Rules in frontmatter, and Imports in their manifest entry. Assignment
uses the Item ID, so a rename is local to the repo and the ID moves with the item.

## Consequences

- Toggles for unknown Item IDs are inert. The server accepts toggle writes
  without checking item existence and never infers deletions from git diffs.
  This supersedes the push cleanup and toggle sweeping in
  [ADR 0005](0005-join-code-scopes.md) and removes the draft-discard protocol.
- An orphan list shows stale toggles. Only an explicit prune clears them.
- Item IDs are always explicit; none is derived from a kind and name. The
  daemon and mutating commands assign a fresh ULID to items with a missing
  ID. The write appears in the Draft. Until then, read-only callers see a
  null ID and sync uses the Harness defaults for that item, so an item
  re-created by hand starts from the defaults, like one re-created through
  sk1ll.
- A daemon refresh first finishes a pending Pull, then writes missing IDs,
  then syncs and reports. It never writes IDs into a pending Pull's partial
  tree, and it does not sync while IDs cannot be written.
- A Machine's reported inventory is incomplete while Item ID metadata is
  unreadable, an item has no ID yet, two items share one, or a Pull is
  pending. The server refuses orphan pruning and default changes that
  record existing items' values while an inventory it needs is incomplete.
- Item ID claims must be unique across the repo. Of the items sharing an ID,
  `sk1ll repair-ids` keeps it with the one that has it at its path in the
  Baseline, so a copy gets the fresh ID and the original keeps its
  Assignment toggles and placements. Only when no claimant (a copy of a
  renamed or uncommitted item) or several (a Baseline that already shares
  it) has it there does the first of them by path keep it. The others get
  fresh IDs, then every item without an ID gets one. A renamed item is its
  ID's only claimant, so it keeps it.
- Nothing writes Item IDs, syncs Harnesses, renames items or pushes while a
  Pull is pending: core refuses it, so no caller can forget. A Harness change
  is still saved then; its placements wait until the Pull is finished.
- The Baseline always gives every item an Item ID of its own. The server's
  pre-receive hook refuses a push whose new tree has an item without one, two
  items sharing one, or unreadable Item ID metadata, naming each item. It
  reads the tree with the same inventory code as the server and the Daemon.
  The server's Baseline inventory relies on that: a Baseline without complete
  IDs (one written on the server by hand, say) is unusable for orphan
  listing, pruning and default changes, instead of having its ID-less items
  silently left out.
- A rename carries the explicit ID with the item. Recovery
  records the placed Item ID so Adopt can find a renamed item or bring back a
  deleted one.
