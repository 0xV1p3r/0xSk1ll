---
name: sk1ll
description: Manage the user's agent skills with the sk1ll CLI. Use when the user asks to install, add or import a skill from a link or owner/repo (e.g. "add this skill https://github.com/...", "install the pdf skill from owner/repo"), to update a skill, or asks what skills they have.
---

# sk1ll

The user's skills live in one sk1ll repo and are synced into every agent
harness. Use the `sk1ll` CLI for them; never copy skill files by hand.

## Add a skill

```sh
sk1ll add <link>                  # GitHub repo, folder or SKILL.md link, or github:owner/repo
sk1ll add --list <link>           # list the skills a multi-skill repo offers; adds nothing
sk1ll add <link> --path <path>    # pick one of those skills
sk1ll add <link> --name <name>    # add it under another name
```

- Write a bare `owner/repo` as `github:owner/repo`.
- If `add` says the link has several skills, run `sk1ll add --list <link>`,
  choose the one that matches what the user asked for and add it with
  `--path`. If none clearly matches, show the list and ask.
- If the name is taken (`already exists`, or `taken` in `--list`), add it
  with `--name <other-name>` and tell the user the name you chose.
- Add right away; don't ask for confirmation first.
- It prints `added NAME @ COMMIT`. The skill lands in this machine's Draft
  (not shared with other machines); the daemon syncs it into the harnesses
  (without a daemon, run `sk1ll sync`). Report the name, the source and the
  commit.

## Look and update

```sh
sk1ll list                # skills and instruction rules
sk1ll status              # import state and pending sync actions
sk1ll update <name>       # re-fetch an import from its source into the Draft
```

`update` refuses to overwrite local edits; tell the user instead of forcing it.

## Rules

- Only use a link or `owner/repo` the user gave you. Never search the web
  for skills to install.
- Never run `commit`, `push`, `restore`, `remove`, `delete`, `rename`, `fork`
  or `sync --force`. Those are the user's decisions; tell them the command if
  they want it.
