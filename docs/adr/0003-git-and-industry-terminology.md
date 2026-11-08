# Server, rules and git verbs replace hub, fragments, promote and reset

We renamed four terms that hid what they mean:

- **Hub → Server.** "Hub" suggested a fan-out relay, not the self-hosted coordinator that is also the fleet's git remote (ADR 0002). "Server" is what people guess right. To keep it unambiguous, *server* only ever means the fleet server; the daemon's endpoint is its *local API*.
- **Fragment → Rule.** "Fragment" described how the file is spliced in, not what it is. "Rule" is what harnesses already call these files (Claude Code, Cursor), so it needs no explanation. *Instructions* stays the name for the rendered whole.
- **Promote → Commit, Reset → Restore.** These actions are git operations, and naming them after git tells users what happens. `reset` was the wrong git verb (`git reset` moves HEAD); this action is `git restore`. Push, Pull and Diff already had git names.

Draft and Baseline stay: they are fleet-level concepts behind the machine statuses, which plain git words don't cover.

## Consequences

- This is a clean break before the first release: the `sk1ll-hub` crate, the `SK1LL_HUB_*` environment variables, the `hub_url` state field, the `fragments/` directory, `fragment:<name>` item references and the `promote`/`reset` commands have no compatibility aliases. Existing test fleets must rejoin, and existing repos must `git mv fragments rules`.
- Users who know git may expect full git behavior. Commit only covers the named items, and Pull blocks instead of merging; both are documented under their glossary entries.
