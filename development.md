# Development notes

Product architecture and dual-process design:
**[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**.

Optional Bunny.net deploy guidance (for self-hosters who want that shape):
**[docs/DEPLOY-BUNNY.md](docs/DEPLOY-BUNNY.md)**.

Upgrades: **[docs/UPGRADE.md](docs/UPGRADE.md)**.

Contributor / agent build commands and invariants: **[AGENTS.md](AGENTS.md)**.

Automated verification (no Discord tokens):

```bash
./scripts/smoke-dual-process.sh
# or the individual cargo test / clippy / build steps in AGENTS.md
```

Keep secrets, production domains, and operator-specific promote steps out of
this repository — document those in your own ops environment.
