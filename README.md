# ownfoil-rs

Barebones Rust port of the core OwnFoil content-server behavior, focused on CyberFoil/Tinfoil-compatible catalog listing and file download.

![ownfoil-rs banner](ownfoil-rs/assets/banner.png)

## Repository Layout

- `ownfoil-rs/`: Rust application crate (server code, auth example, parity notes)
- `compose.dev.yaml`: local/dev compose stack
- `Dockerfile.dev`: dev build image used by compose

## Features

- Minimal HTTP API for shop/catalog/title-version browsing
- File streaming with single-range `Range` support (`206 Partial Content`)
- Optional HTTP Basic auth (`Authorization: Basic ...`)
- Dedicated auth credentials file support (`--auth-file`)
- Private-by-default startup (requires auth file unless public mode is explicitly enabled)
- Recursive scan of a content library root (`.nsp`, `.xci`, `.nsz`, `.xcz`)
- Background catalog refresh interval
- CyberFoil-compatible shop endpoints (`/`, `/api/shop/sections`, `/api/get_game/:id`)

## Run

```bash
cargo run -p ownfoil-rs -- \
  --bind 0.0.0.0:8465 \
  --library-folder ./library \
  --auth-file ./auth.toml \
  --scan-interval-seconds 30
```

Verbose logs:

```bash
RUST_LOG=debug cargo run -p ownfoil-rs -- --library-folder ./library
```

### Run with Docker Compose (dev)

```bash
cp .env.example .env
# set LIBRARY_FOLDER and AUTH_FILE in .env
docker compose -f compose.dev.yaml up --build
```

The compose setup mounts:
- content library to `/library` (read-only)
- auth file to `/config/auth.toml` (read-only)

### Config file (optional)

```toml
bind = "0.0.0.0:8465"
library_root = "./library"
auth_file = "./auth.toml"
scan_interval_seconds = 30
```

Example credentials file is included at `ownfoil-rs/auth.example.toml`.
`auth.toml` format:

```toml
# single user
username = "admin"
password = "change-me"

# optional additional users
[[users]]
username = "friend"
password = "friend-pass"
```

Run with config file:

```bash
cargo run -p ownfoil-rs -- --config ./ownfoil.toml
```

CLI flags override config file values.
`--library-root` is also accepted as a compatibility alias.

### Public mode (optional)

By default, the server starts in private mode and requires an auth file.

To run without authentication, set:

```bash
OWNFOIL_PUBLIC=true cargo run -p ownfoil-rs -- --library-folder ./library
```

The same setting works in compose through `.env`:

```bash
OWNFOIL_PUBLIC=true
```

## Expected Library Structure

`--library-folder` can contain nested directories. Any files ending in `.nsp`, `.xci`, `.nsz`, `.xcz` are indexed.

Example:

```text
library/
  Content Pack A/
    Main Content [1234567890123456][v0].nsp
    Update Content [1234567890123466][v131072].nsp
    Extra Content [1234567890123477][v0].nsp
  Content Pack B/
    Main Content [2234567890123456][v0].xci
```

For best update/DLC behavior in CyberFoil:
- Ensure update and DLC files include their own content identifier in filename or path.
- Subdirectories are fully supported.
- The shop payload maps:
  - `title_id` => base content identifier
  - `app_id` => concrete content id (base/update/dlc)
  - `app_type` => `BASE` / `UPDATE` / `DLC`

## API Surface

- `GET /health`
- `GET /` (Tinfoil/CyberFoil root payload: `success` + `files`)
- `GET /api/catalog`
- `GET /api/sections`
- `GET /api/sections/:section` where `section in {new,recommended,updates,dlc,all}` (legacy compatibility aliases are also supported)
- `GET /api/shop/sections?limit=<n>` (Ownfoil/CyberFoil-style sections with nested `items`)
- `GET /api/search?q=<text>`
- `GET /api/title/:content_id/versions`
- `GET /api/download/*path`
- `GET /api/get_game/:id`

Compatibility aliases:

- `GET /shop`, `GET /index`, `GET /titles`
- `GET /api/shop`, `GET /api/index`, `GET /api/titles`
- `GET /download/*path`

## Client Setup (Tinfoil/CyberFoil)

1. Start server and ensure it is reachable from your device.
2. In client custom shop settings, point index/shop URL to:
   - `http://<server-ip>:8465`
3. If auth is enabled, provide matching username/password so the client sends HTTP Basic auth.
4. Verify content downloads resolve to `/api/get_game/:id#filename` and support resume/range requests.

## Notes

- Filename parsing extracts content identifier/version heuristically (for example, patterns like `[1234567890123456][v123]`).
- This project does not decrypt/encrypt shop payloads; responses are plain JSON.

## Thanks to

- CyberFoil: https://github.com/luketanti/CyberFoil
- Ownfoil (luketanti fork): https://github.com/luketanti/ownfoil
- Ownfoil (original by a1ex4): https://github.com/a1ex4/ownfoil
