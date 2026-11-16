# Security

## Reporting a vulnerability

Please don't open a public issue. Report it privately through
[GitHub's private vulnerability reporting](https://github.com/0xV1p3r/0xSk1ll/security/advisories/new),
with the affected version (`sk1ll --version`), what an attacker needs and
what they can do.

## Supported versions

Only the latest release gets fixes.

## The trust model in brief

- The server trusts every joined machine: any machine token can manage the
  whole fleet, including creating join codes and removing machines. The
  admin token can do the same and is only needed to bootstrap.
- The server speaks plain HTTP. Put a TLS proxy in front of it whenever
  machines reach it over a network you don't trust.
- The daemon's local API is a Unix socket only its user can open.
- Imported skills are content written by their authors and run by your
  agents. Review an import (`sk1ll diff`) before you commit it.

[Known limitations](docs/known-limitations.md) lists the edge cases sync's
safety guarantees don't cover.
