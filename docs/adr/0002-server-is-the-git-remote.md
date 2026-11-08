# The server is the fleet's git remote

The server (a self-hosted Docker container) hosts the canonical bare repo, and machines push and pull only through it. GitServer or another remote is at most a backup mirror. Because every commit lands at the server, the server can tell the other machines to pull immediately. Git stays the content store and history, with no second content format. Each machine keeps a full local clone, which holds its draft and lets it keep working while the server is unreachable.

## Considered Options

- GitServer as remote, with the server only relaying status: it gives no instant propagation without webhooks, and leaves two endpoints to configure per machine.
- Native content storage in the server: it duplicates what git already does well.
