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

`type` is `personal`, `import`, `fragment`, or `unmanaged`. `item` is
`skill:<name>` or `fragment:<name>`, and is null for unmanaged Skills.
`description` is a string or null. `draft` is `added`, `modified`, `deleted`,
or null. `in_baseline` refers to the applied fleet Baseline, so an unpushed
promote can still have `in_baseline: false` and `draft: null`.

`path` is the absolute Skill directory or Fragment file in the repo. It can
name a missing path for a deleted Draft item. Unmanaged entries have a null
`path`; each location supplies its own absolute path instead.
`enabled_harnesses` uses this Machine's current Assignment and the Harness's
supported content types. A Fragment's frontmatter restricts it further. It is
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
      ]
    }
  ]
}
```

Identical copies share one variant. Multiple variants mean the content differs.
The entry's description comes from its first variant; each variant also has its
own description. `clashes_with_repo` means a repo Skill already has this name.
Repo entries always have an empty `variants` array and `clashes_with_repo: false`.

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
  "binary": false,
  "too_large": false,
  "size": 123
}
```

Both endpoints require `item`. Use `fragment:git` to browse a Fragment, whose
single file is named `git.md`. For unmanaged Skills use `unmanaged:pdf`, with
`hash=sha256:...` to choose a variant. Hash is required when variants differ.
Optional `harness=claude` chooses a location within that variant; otherwise its
first location is used. URL-encode query values.

`path` is required for file reads and is relative to the Skill folder. A
Fragment accepts only its own filename. Absolute paths, parent components and
symlinks are refused on Unix. Other platforms use best-effort containment checks. Invalid UTF-8 or NUL bytes set
`binary: true` and `text: null`. Files larger than 1 MiB set `too_large: true`
and `text: null`; their binary status is not inspected.

Repo reads use Draft content on disk. Fully deleted Draft items use their
Baseline version from Git `HEAD`. Deleting only `SKILL.md` still exposes any
remaining Draft files. Unmanaged reads use the selected Harness location.

## Adopt and Fork

`POST /skills/adopt` takes:

```json
{"harness": "claude", "name": "pdf"}
```

Success is HTTP 204 with no body. The existing `POST /conflicts/adopt` accepts
the same body and also supports unmanaged Skills.

Unmanaged Adopt requires a running Daemon connected to the Hub. Otherwise it
returns HTTP 503 with `"Adopt needs a connection to the Hub"`, without changing
the repo or Harnesses. The GUI disables Adopt and Adopt to edit while disconnected.
On a joined repo, the CLI also requires the connected Daemon and suggests
`sk1ll daemon` when it is stopped. Standalone repos use local Assignment.

Adopt selects the chosen variant and its identical copies across Harnesses.
It refuses an existing repo Skill with different content or an Import with the
same name. An identical personal copy is accepted so an interrupted Adopt can
be retried. Differing Harness copies stay unmanaged.

The Hub rejects toggles for unknown items, so Adopt first copies the selected
content into `personal/<name>/` and reports the Draft. It waits for the Hub to
see it, then synchronously sets explicit `enabled: true` Assignment toggles in
each Harness where that variant was found. A rejected Assignment rolls back
only the repo copy this request created, and only while it still matches a
source. Existing copies and concurrent edits are preserved. A crash between
publication and Assignment leaves the original Harness copies untouched;
retrying Adopt finishes the Assignment. Explicit enable toggles are idempotent.

After the Hub accepts Assignment, sync replaces the selected Harness copies
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
