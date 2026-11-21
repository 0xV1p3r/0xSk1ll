# Join codes grant a scope: the whole fleet or only the machine itself

Every joined machine used to manage the whole fleet. That suits one person's machines, but not a shared or less trusted machine, such as a CI runner or a colleague's laptop. A join code now grants one of two scopes, stored per machine at the server and checked in every handler:

- `fleet` (the default, and what every machine had before) may create join codes, change any machine's assignment, undo any log entry and remove any machine.
- `self` may change only its own assignment, undo log entries for itself (whoever wrote them) and remove itself. It may not create join codes, so it can't mint a `fleet` code for itself or anyone else.

Every machine may still read the fleet view, every assignment, the log and the imports view. The admin token keeps full rights. A machine that is removed and joins again gets the scope of its new code.

A `self` machine may remove itself. Leaving touches only its own assignment and token, the log keeps a record, and coming back needs a new code from a `fleet` machine or the admin. It lets a machine be retired from that machine without granting anything over the others.

## Consequences

- Scopes cover fleet administration. A `self` machine can still push content that every machine pulls.
- The server checks the token and scope again inside each write transaction. Removing a machine takes the push lock. A queued push checks its token after taking that lock and is refused if the machine was removed.
- A daemon session registers under the same database lock as removal, after checking its token. Removal revokes the session holding the removed token, which protects a machine that has already rejoined.
- The server takes pushes only to the Baseline branch and ignores replacement objects in its own git reads.
- A `self` machine can prune its own orphan toggles. A `fleet` machine or the admin can prune across the Fleet. See [ADR 0006](0006-items-have-stable-ids.md).
