# Skills API

These endpoints use the Daemon's local Unix socket. Shared Rust types are in
`sk1ll_core::content` and re-exported by `sk1ll_daemon::client`.

## Inventory

`GET /skills` returns an array of entries with this exact shape:

```json
{
  "name": "pdf",
  "item": "skill:pdf",
  "id": "01KAC9SRJZ9Z5SSWRBQE76YS69",
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
`skill:<name>` or `rule:<name>`, and is null for unmanaged Skills. The Item ID
field `id` is omitted for unmanaged Skills. It is null for an item that has no
Item ID yet; the next reconcile writes one, and until then the item follows the
Harness defaults.
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
size zero. Tree browsing does not follow directory symlinks. Working-tree
results omit `.git`, `.sk1ll-tmp` and a skill's root `.sk1ll-placement` file.

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

A successful save returns `204 No Content` and reconciles the Draft and
Harnesses. A stale fingerprint returns `409 Conflict`. To replace the new
disk contents, read the file again and use its new fingerprint. See
[Known limitations](known-limitations.md#a-concurrent-save-in-the-library-editor)
for a write that lands after the final fingerprint check.

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
Without a running Daemon, the CLI's own engine connects to the Server first
and refuses the same way when it can't. Standalone repos use local Assignment.

Adopt selects the chosen variant and its identical copies across Harnesses.
It refuses an existing repo Skill with different content or an Import with the
same name. An identical personal copy is accepted so an interrupted Adopt can
be retried: that variant is `resumable` in `GET /skills`, and the Library offers
Resume adoption for it. Differing Harness copies stay unmanaged.

Adopt copies the selected content into `personal/<name>/` with a new Item ID. The Daemon sends a status with that ID before sending the Assignment request on the same WebSocket session. The request explicitly enables the item for each Harness with a matching copy. A definitive refusal rolls back only a copy created by that request, and only if it still matches a source. If the connection drops before the reply, the Draft keeps its ID so Adopt can retry.

After the Server accepts Assignment, sync replaces the selected Harness copies
through normal displacement, retaining originals in recovery. Other Harnesses
follow their new-Skills defaults. Success is returned after these steps finish.
There is no offline adoption queue or background adoption replay.

Refresh on the existing `draft`, `status` and `fleet` SSE events at `GET /events`.
`GET /draft`, `/items`, `/conflicts` and `/conflicts/displaced` answer from
the Daemon's last reconcile, brought up to date first unless an engine
operation is running; then they answer at once with the previous state, and
the `draft` or `status` event follows the reconcile that catches up. Add
`?fresh=true` to act on what is listed: the read then waits for a running
engine operation and reconciles first. The CLI does so for `restore --all`,
`conflicts` and `recover list`; the GUI keeps reads that never wait.
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

`POST /items/rename` renames a personal Skill or Rule in the Draft.

```json
{"kind": "skill", "name": "pdf", "to": "documents"}
```

It returns `{"item":"skill:documents"}`. The rename keeps the Item ID, so the
Assignment follows the item on every Machine. Restore both Draft entries to
undo a rename.

### Assignment writes

`GET /items` includes the `id` of each Skill and Rule for the Assignment matrix.
`PUT /machines/{id}/assignment` accepts `changes` and an optional `expected`
revision and registration. An item toggle takes `harness`, `kind`, `item_id`,
`enabled` and the optional display `name`. A null `enabled` clears the explicit
toggle. A default change takes `kind: "new_skills_default"` or
`"new_rules_default"` and a boolean `enabled`.

A joined Daemon sends its own writes as `set_assignment` frames after the latest
status. The server replies with `assignment_result`, the request's `req`, and the
last status `seq` it processed for the session. Admin and cross-Machine writes use the server's REST API. Unknown Item IDs are accepted and inert.

The status carries `inventory_complete` and an optional `inventory_error`.
An inventory is incomplete while Item ID metadata is unreadable, an item has
no Item ID yet, two items share one, or a Pull is pending (the Daemon then
keeps reporting its last inventory).
When an item is replaced with a different ID under the same name, `GET /items`
returns separate Baseline and Draft rows.

`POST /assignment/harness/{harness}` takes `{"enabled":false}` and sets both
defaults and every explicit toggle for that Harness. It returns the
Assignment update, including any local placement errors.

## Sync and instruction previews

`GET /harnesses/{name}/render` uses this Machine's Draft and Assignment.
It returns an array of files with `path`, `format` (`block` or `rules`),
`content` (the rendered rules that `sk1ll render` prints), and
`next_content` (the complete proposed file, including text outside a managed
block). `differs` compares the proposed file with the file on disk. `diff`
is a unified diff, or an empty string when the file matches. A diff can
include proposed changes that sync will leave for conflict resolution.

`POST /sync` with `{"dry_run":true}` returns `actions`, `conflicts` and a `fingerprint`
without applying changes or running recovery. Each action has `harness`,
`kind` (`create`, `update`, `remove`, or `conflict`), `item`, `name`,
`dest`, and `reason`. `GET /sync` returns the same plan. Both endpoints accept
an optional `force` boolean, which defaults to false. Use GET when a Daemon
might predate dry-run support.

A regular `POST /sync` replans and applies. Supply the preview’s `fingerprint` to apply a reviewed plan. The Daemon checks it under the engine lock after recovering interrupted writes. If the plan changed, it returns 409 with the fresh preview. Copy-mode placements
are hashed after staging. A mismatched copy stays unpublished and also returns
409 with a fresh preview. Earlier placements may already have applied.
A completed sync returns remaining `conflicts`, the recovery entries that
hold a change (`kept`, such as each hand edit a forced sync overwrote) and
any placement `errors`.

## Conflicts and recovered content

`GET /conflicts` lists Harness conflicts: Harness content that differs from
the Draft, to adopt or overwrite. `GET /conflicts/displaced` lists every
recovery entry, each with an `id` to adopt or discard. An entry with
`changed` holds a change (a hand edit that was overwritten, a save during a
swap) and needs the user; one without it is as sk1ll set it aside and is
safe to discard. `adoptable` is false where Adopt can't tell which item the
content belongs to (a skill link, or content with no record of where it came
from); `POST /conflicts/diff` with its `id` then compares it with nothing.
The original of an adopted unmanaged skill goes back into that skill. The
Machine's report counts Harness conflicts and changed entries, by item and
Harness: that is the Conflict status, the Fleet's Conflicts column and the
app's badge.

## Harness change and import update preconditions

`POST /harnesses/preview` returns a `fingerprint` for the config snapshot.
Supply it with the same `change` to `POST /harnesses/apply`. Apply returns
409 if the file changed and 400 with an update prompt if the client omits the
fingerprint. `GET /harnesses/config/path` returns the configured path as a JSON
string and works even when the file cannot be parsed.

Send `POST /imports/{name}/update-reviewed` with `{"expected":"<sha>","force":false}`
to check the upstream commit resolved during the fetch. It returns 409 if the
commit differs. Set `force` to `true` to overwrite local edits. Clients require
this route for reviewed updates and prompt for a Daemon restart if the route
is missing. Regular updates use `POST /imports/{name}/update` and can take
`?force=true`.
