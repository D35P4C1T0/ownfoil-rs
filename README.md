# ownfoil-rs

Rust rewrite of [Ownfoil](https://github.com/a1ex4/ownfoil): a self-hosted Nintendo
Switch game-shop library for Tinfoil, Aerofoil/CyberFoil, Sphaira, and browsers.
Parity work targets Ownfoil v2.4.1, pinned upstream commit `0cce4bbc684b30930b1576847c8c8fb5202114bf`.

Current release: **v0.3.0**.

![ownfoil-rs banner](ownfoil-rs/assets/banner.png)

## Features

- NSP, NSZ, XCI, and XCZ discovery with console-key CNMT identification and
  filename fallback
- Durable SQLite catalog, users, scan state, stable file IDs, and migration from
  an existing Ownfoil database
- Multiple libraries, watcher, scheduler, organization templates, dry-run preview,
  crash journal, and old-update cleanup
- Ownfoil TitleDB download/cache with titles, versions, icons, banners, languages,
  missing content, and completeness status
- Tinfoil encrypted shops and `Hauth`, CyberFoil JSON, Sphaira directory listings,
  browser library/setup/settings pages, and resumable byte-range downloads
- Scrypt users with separate admin, shop, and backup roles; public or private shops
- Ownfoil-compatible YAML settings and Docker volume layout
- GraphQL queries and admin mutations; live task/worker events and statistics
- Durable background jobs, configurable watcher, and TitleDB scheduling
- Rust NSZ/XCZ solid and block compression, decompression, signature and hash
  verification; no Python helpers or C Zstandard dependency

See [ROADMAP.md](ROADMAP.md) for the feature ledger and remaining
hardware/deployment validation gates. Intentional differences are documented in
[ownfoil-rs/PARITY_NOTES.md](ownfoil-rs/PARITY_NOTES.md).

## Docker Compose

Copy the environment template, set your game directory and initial administrator,
then start the service:

```bash
cp .env.example .env
# Edit GAMES_PATH and replace the example password in .env.
docker compose up --build -d
```

For a one-off start without an `.env` file:

```bash
GAMES_PATH=/mnt/iso \
USER_ADMIN_NAME=admin \
USER_ADMIN_PASSWORD='replace-this-password' \
OWNFOIL_INSECURE_ADMIN_COOKIE=true \
docker compose up --build -d
```

The production layout is:

- `/games` — game libraries
- `/app/config/settings.yaml` and `/app/config/keys.txt`
- `/app/data/ownfoil.db` and TitleDB cache
- HTTP port `8465`

`GAMES_PATH`, `CONFIG_PATH`, `DATA_PATH`, `OWNFOIL_PORT`, `PUID`, and `PGID` are
configurable through `.env`. Game storage is writable by default so uploads and
the organizer work; set `GAMES_MOUNT_MODE=ro` for a serving-only library. Set
`USER_ADMIN_NAME` and `USER_ADMIN_PASSWORD` for first-start admin bootstrap, or
create the first administrator in the browser. Optional guest bootstrap uses
`USER_GUEST_NAME` and `USER_GUEST_PASSWORD` after an administrator exists.

Admin cookies are secure by default. Set `OWNFOIL_INSECURE_ADMIN_COOKIE=true` only
when accessing the admin UI directly over plain HTTP; leave it unset behind HTTPS.

Useful lifecycle commands:

```bash
docker compose ps
docker compose logs -f ownfoil
docker compose stop
docker compose down
```

## Native Run

Rust 1.92 or newer is required.

```bash
cargo run -p ownfoil-rs -- \
  --bind 0.0.0.0:8465 \
  --library-folder ./library \
  --settings ./config/settings.yaml
```

On a native checkout, data is stored under `./data`; container defaults activate
when the settings directory is `/app/config`. CLI/runtime TOML values are startup
overrides, while `settings.yaml` is the canonical mutable Ownfoil configuration.
Legacy `auth.toml` credentials can still be imported with `--auth-file`.

Point a client at `http://<server-ip>:8465`; use `/base`, `/update`, `/dlc`, or
`/multi` for filtered shops. Visit `/setup` for host-specific instructions and
`/settings` for administration.

## Helm

The chart in `chart/` provides separate persistent volumes for configuration,
data, and games:

```bash
helm upgrade --install ownfoil ./chart \
  --set image.repository=ghcr.io/d35p4c1t0/ownfoil-rs \
  --set image.tag=v0.3.0
```

Review `chart/values.yaml` before deployment, especially persistence, ingress,
resource limits, and bootstrap environment variables.

## Development

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

Run `./scripts/setup-hooks.sh` once to enable the repository pre-push checks.

See [VALIDATION.md](VALIDATION.md) for the v0.3.0 validation record.

## Attribution

Behavioral parity is based on Ownfoil by a1ex4. See `LICENSE` and preserve the
project's AGPL source-availability obligations when distributing modified builds.
