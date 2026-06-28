# Deployment topology — read before touching "production"

This bot's real, live production instance runs **locally on this same machine**, not on a
remote server. Verify the facts below by inspecting the live system (`sudo docker ps -a`,
`docker inspect`, `ss -tlnp`) before assuming anything about "production" — the repo's
`docker-compose.yml` and CI workflows do **not** fully describe what's actually deployed.

## What's actually running

- The live bot is the Docker container named `dickgrowerbot` (image `kozalosev/dickgrowerbot`,
  built locally from this repo's `Dockerfile` — **not** pulled from a registry), managed by the
  `docker-compose.yml` in this repo root (compose project name `dickgrowerbot`).
- Its real database is the container `dickgrowerbot-postgresql` (**Postgres 14.5**), reachable
  only from inside the Docker network `dickgrowerbot_postgres-network` via the DNS alias
  `postgres:5432` — it has **no port published to the host**. You cannot reach it via
  `localhost`.
- `localhost:5432` on this host is a **completely different, native Postgres 16 installation**,
  used only for local dev/testing (`cargo test`, `cargo sqlx prepare`, `sqlx migrate run`). It
  is NOT production data, even though it may contain realistic-looking seeded data. Confirm via
  `ps aux | grep postgres` (look at the version in the binary path) if ever in doubt.

## What does *not* deploy anything here

- `docker-compose.yml`'s `image: kozalosev/dickgrowerbot` refers to the **upstream** repo's
  image name. If you're working in a fork (e.g. `tmigliorini/IMDILcazzobot`), pushing tags here
  triggers *that fork's own* `.github/workflows/publish.yaml`, which builds and publishes to
  `ghcr.io/<fork>/<repo>` — a completely different image than the one actually running locally.
  Watchtower (labeled on the container) would only ever notice updates to the exact image name
  the container already runs (`kozalosev/dickgrowerbot`), not the fork's GHCR image. **Pushing
  to GitHub and cutting a release tag does not deploy anything to this local instance.**
- GitHub Actions are disabled by default on forks until manually enabled once in the repo's
  Actions tab. A push/tag before that point produces zero workflow runs, silently.

## How to actually deploy a local change to the running bot

From `/home/ubuntu/DickGrowerBot`, rebuild and recreate **only** the bot service, never the
database:

```
sudo docker compose build DickGrowerBot
sudo docker compose up -d DickGrowerBot
```

This causes a brief restart of the live Telegram bot. Never run a bare `docker compose down`
or `up` without scoping it to `DickGrowerBot` specifically — the same compose file also owns the
`postgres` service backing real user data, and `down -v` would destroy it.

## Before any production-affecting action

Confirm explicitly with the user before running the build/restart above, even if they already
asked generically for "deploy to production" earlier in the conversation — confirm the specific
command, since the topology above is easy to get wrong and the blast radius is a live bot with
real user balances.
