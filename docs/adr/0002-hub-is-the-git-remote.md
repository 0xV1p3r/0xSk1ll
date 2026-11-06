# The hub is the fleet's git remote

The hub (a self-hosted Docker container) hosts the canonical bare repo, and machines push and pull only through it. GitHub or another remote is at most a backup mirror. Because every promote lands at the hub, the hub can tell the other machines to pull immediately. Git stays the content store and history, with no second content format. Each machine keeps a full local clone, which holds its draft and lets it keep working while the hub is unreachable.

## Considered Options

- GitHub as remote, with the hub only relaying status: it gives no instant propagation without webhooks, and leaves two endpoints to configure per machine.
- Native content storage in the hub: it duplicates what git already does well.
