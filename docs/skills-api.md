# Skills API

These endpoints use the Daemon's local Unix socket. Shared Rust types are in
`sk1ll_core::content` and re-exported by `sk1ll_daemon::client`.

## Inventory

`GET /skills` returns an array of entries with this exact shape:

```json
{
  "name": "pdf",
  "item": "skill:pdf",
  "type": "personal",
  "description": "Work with PDFs",
  "draft": "added",
  "in_baseline": false,
  "path": "/home/me/skills/personal/pdf",
  "enabled_harnesses": ["claude", "codex"],
  "clashes_with_repo": false,
  "variants": []
}
```

`type` is `personal`, `import`, `rule`, or `unmanaged`. `item` is
`skill:<name>` or `rule:<name>`, and is null for unmanaged Skills.
`description` is a string or null. `draft` is `added`, `modified`, `deleted`,
or null. `in_baseline` refers to the applied fleet Baseline, so an unpushed
commit can still have `in_baseline: false` and `draft: null`.

`path` is the absolute Skill directory or Rule file in the repo. It can
name a missing path for a deleted Draft item. Unmanaged entries have a null
`path`; each location supplies its own absolute path instead.
`enabled_harnesses` uses this Machine's current Assignment and the Harness's
supported content types. A Rule's frontmatter restricts it further. It is
empty while the authoritative Assignment is unavailable and for unmanaged
entries.

An unmanaged entry uses the same fields:

```json
{
  "name": "pdf",
  "item": null,
  "type": "unmanaged",
  "description": "Work with PDFs",
  "draft": null,
  "in_baseline": false,
  "path": null,
  "enabled_harnesses": [],
  "clashes_with_repo": false,
  "variants": [
    {
      "hash": "sha256:...",
      "description": "Work with PDFs",
      "locations": [
        {"harness": "claude", "path": "/home/me/.claude/skills/pdf"},
        {"harness": "codex", "path": "/home/me/.codex/skills/pdf"}
      ],
      "resumable": false
    }
  ]
}
```

Identical copies share one variant. Multiple variants mean the content differs.
The entry's description comes from its first variant; each variant also has its
own description. `resumable` marks the variant identical to an existing
personal copy of the same name, which an interrupted Adopt left behind (see
[Adopt and fork](#adopt-and-fork)). `clashes_with_repo` means a repo Skill
already has this name and no variant is resumable. When one is, only the
resumable variants can be adopted. Repo entries always have an empty
`variants` array and `clashes_with_repo: false`.

## Tree and text

`GET /skills/tree?item=skill:pdf` returns a flat array, sorted by path traversal:

```json
[
  {"path": "SKILL.md", "kind": "file", "size": 123},
  {"path": "references", "kind": "directory", "size": 0},
  {"path": "references/example.md", "kind": "file", "size": 42}
]
```

`kind` is `file`, `directory`, or `symlink`. Sizes are bytes; directories have
size zero. Tree browsing does not follow directory symlinks. Sk1ll markers and
`.git` directories are omitted from working-tree results.

`GET /skills/file?item=skill:pdf&path=SKILL.md` returns:

```json
{
  "path": "SKILL.md",
  "text": "---\ndescription: Work with PDFs\n---\n...",
  "fingerprint": "sha256:…",
  "binary": false,
  "too_large": false,
  "size": 123
}
```

Both endpoints require `item`. Use `rule:git` to browse a Rule, whose
single file is named `git.md`. For unmanaged Skills use `unmanaged:pdf`, with
`hash=sha256:...` to choose a variant. Hash is required when variants differ.
Optional `harness=claude` chooses a location within that variant; otherwise its
first location is used. URL-encode query values.

`path` is required for file reads and is relative to the Skill folder. A
Rule accepts only its own filename. Absolute paths, parent components and
symlinks are refused on Unix. Other platforms use best-effort containment checks. Invalid UTF-8 or NUL bytes set
`binary: true` and `text: null`. Files larger than 1 MiB set `too_large: true`
and `text: null`; their binary status is not inspected.

Repo reads use Draft content on disk. Fully deleted Draft items use their
Baseline version from Git `HEAD`. Deleting only `SKILL.md` still exposes any
remaining Draft files. Unmanaged reads use the selected Harness location.

### Save a file

`POST /skills/file` takes a JSON body with the item, relative path, new text
and the fingerprint returned by the read:

```json
{
  "item": "rule:git",
  "path": "git.md",
  "content": "# Git\n",
  "expected": "sha256:…"
}
```

The fingerprint is null for Baseline fallback and when there is no text preview. Saves require an
existing UTF-8 text file of at most 1 MiB in a Personal Skill or Rule. Rule
frontmatter must parse. Fork an Import or Adopt an Unmanaged skill before
editing. The Daemon refuses symlink traversal on writes.

A successful save returns `204 No Content` and refreshes the Draft and
Harnesses. A stale fingerprint returns `409 Conflict`. To replace the new
disk contents, read the file again and use its new fingerprint.

## Adopt and fork

`POST /skills/adopt` takes:

```json
{"harness": "claude", "name": "pdf"}
```

Success is HTTP 204 with no body. The existing `POST /conflicts/adopt` accepts
the same body and also supports unmanaged Skills.

Unmanaged Adopt requires a running Daemon connected to the Server. Otherwise it
returns HTTP 503 with `"Adopt needs a connection to the Server"`, without changing
the repo or Harnesses. The GUI disables Adopt and Adopt to edit while disconnected.
On a joined repo, the CLI also requires the connected Daemon and suggests
`sk1ll daemon` when it is stopped. Standalone repos use local Assignment.

Adopt selects the chosen variant and its identical copies across Harnesses.
It refuses an existing repo Skill with different content or an Import with the
same name. An identical personal copy is accepted so an interrupted Adopt can
be retried: that variant is `resumable` in `GET /skills`, and the Library offers
Resume adoption for it. Differing Harness copies stay unmanaged.

The Server rejects toggles for unknown items, so Adopt first copies the selected
content into `personal/<name>/` and reports the Draft. It waits for the Server to
see it, then synchronously sets explicit `enabled: true` Assignment toggles in
each Harness where that variant was found. A rejected Assignment rolls back
only the repo copy this request created, and only while it still matches a
source. Existing copies and concurrent edits are preserved. A crash between
publication and Assignment leaves the original Harness copies untouched;
retrying Adopt finishes the Assignment. Explicit enable toggles are idempotent.

After the Server accepts Assignment, sync replaces the selected Harness copies
through normal displacement, retaining originals in recovery. Other Harnesses
follow their new-Skills defaults. Success is returned after these steps finish.
There is no offline adoption queue or background adoption replay.

Refresh on the existing `draft`, `status` and `fleet` SSE events at `GET /events`.
Adopt emits `draft` after creating the Draft and after finishing replacement.

`POST /imports/fork` already takes:

```json
{"name": "pdf"}
```

It returns HTTP 204 with no body, turning the Import into a personal Skill in
Draft. Errors use the existing non-2xx JSON body:

```json
{"error": "a repo skill named `pdf` already exists"}
```

## Rename

`POST /items/rename` takes:

```json
{"kind": "skill", "name": "pdf", "to": "docs"}
```

It copies this Machine's explicit toggles for the old name to the new one
first, then renames the personal Skill or Rule in the Draft:

```json
{"item": "skill:docs", "migrated": ["claude"], "kept": ["codex"], "assignment_error": null}
```

The copy is made by the Assignment's authority, never described by the
caller: the Server when joined (`POST /api/machines/{id}/assignment/copy`
with `{"from": "skill:pdf", "to": "skill:docs"}`, which this local API does
not expose), the Daemon itself when standalone (skipping toggles for a
Harness this Machine no longer has). In one transaction it checks that `from`
is an item this Machine can have and `to` is the same kind and not one yet,
then copies the current explicit toggles of `from` to `to`, logged like any
change. A Harness where `to` already has an explicit toggle keeps it and is
listed in `kept`; `migrated` lists only the Harnesses whose toggle was
written. The Daemon always asks, even when it sees no toggles to copy, so a
toggle set on the old name just before is not missed. `assignment_error` is
always null; it stays for older clients.

When the copy can't be made, nothing is renamed and the request fails with
HTTP 409 and the reason: offline, the Server refused it, or the Server is
older and has no copy endpoint (HTTP 404, reported as "Assignment copy is not
supported by this Server"). When the copy's response is lost or uncertain, the Daemon
refuses the rename and leaves the Assignment alone. The error says the new
name may already have copied toggles. Those toggles stay unused unless an
item with that name appears. To rename anyway, send `"copy_assignment": false`.
The new name uses any explicit toggles already present, and each Harness's
default otherwise.

When the rename then moves nothing (the new name appeared since it was
checked, say), the Daemon removes the toggles the copy applied again, with the
copy's `expected` revision and registration, so that a toggle someone changed
meanwhile is left alone; the error says whether they were removed or kept, and
in which Harnesses. When the item moved but rewriting its frontmatter `name:`
failed, the rename stands with its copied toggles and the error says so.
Standalone, a saved Assignment counts as written even when applying it fails
afterwards; that failure shows in the status.

Other Machines' toggles are not copied: once the rename is pushed, the old
name's are dropped (`item_removed`) and the new name follows their defaults.
The old name's toggles on this Machine also stay until that push, so restoring
both Draft items undoes the rename. Toggles copied to the new name before the
rename reaches the Draft survive a push meanwhile only because a push sweeps
just the items it removed from the Baseline (delta-only sweeps), which never
include the new name.

### Assignment writes

`PUT /machines/{id}/assignment` (passed through to the Server when joined)
also takes an optional `expected`: `{"revision": 7, "registration": "…"}`.
The write applies only to that revision and, when given, that registration of
the Machine; otherwise it fails with HTTP 409 and changes nothing. Standalone,
a registration never matches.

A 0.1.0 Server ignores it, and leaves `registration` out of `GET /fleet`, so the
GUI offers matrix Undo only when it is there (or standalone). `GET /fleet` lists each Machine's
`registration`, a random id that is new for every join, and the Server's
`GET /machines/{id}/assignment` returns it too.


### Set a Harness's items and defaults

Send `POST /api/machines/{id}/assignment/harness/{harness}` with
`{"enabled": false}` (or `true`) to set both defaults and every Baseline item,
reported draft, and unpushed item for that Machine in one transaction.
It uses the same scope, change log and response as the assignment PUT.

The local endpoint `POST /assignment/harness/{harness}` targets the
Daemon's own Machine and returns the assignment update in `update`, with
`warning` set to null. Standalone uses the local repo. If the Server returns
a 404 without a JSON body, the Daemon uses the local repo and returns a warning
that unpulled items may stay on.

## Sync and instruction previews

`GET /harnesses/{name}/render` uses this Machine's Draft and Assignment.
It returns an array of files with `path`, `format` (`block` or `rules`),
`content` (the rendered rules that `sk1ll render` prints), and
`next_content` (the complete proposed file, including text outside a managed
block). `differs` compares the proposed file with the file on disk. `diff`
is a unified diff, or an empty string when the file matches. A diff can
include proposed changes that sync will leave for conflict resolution.

`POST /sync` with `{"dry_run":true}` returns `actions` and `conflicts`
without applying changes or running recovery. Each action has `harness`,
`kind` (`create`, `update`, `remove`, or `conflict`), `item`, `name`,
`dest`, and `reason`. `GET /sync` returns the same plan. Both endpoints accept
an optional `force` boolean, which defaults to false. Use GET when a Daemon
might predate dry-run support.

A regular `POST /sync` replans and applies. It returns remaining `conflicts`
and includes `errors` if any placements fail.
