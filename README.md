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
- Optional HTTP Basic auth (`Authorization: Basic ...`) with constant-time password comparison
- Strict HTTP Basic scheme parsing (`Authorization` must use `Basic <base64>`)
- Dedicated auth credentials file support (`--auth-file`); warns if file is world-readable (Unix)
- Private-by-default startup (requires auth file unless public mode is explicitly enabled)
- Admin session cookie uses `Secure` by default (set `OWNFOIL_INSECURE_ADMIN_COOKIE=true` only for non-TLS admin access)
- Recursive scan of a content library root (`.nsp`, `.xci`, `.nsz`, `.xcz`) via `walkdir`
- Background catalog refresh interval with panic recovery
- CyberFoil-compatible shop endpoints (`/`, `/api/shop/sections`, `/api/get_game/:id`)
- Rate limiting (20 req/s, burst 50) per client IP
- Request ID propagation (`X-Request-ID` header)
- Graceful shutdown on Ctrl+C
- Config validation (library root and auth file must exist at startup)
- Admin web UI at `/admin` for browsing titles (session auth, auth.toml users)

## Development

Before pushing, run `./scripts/setup-hooks.sh` once to enable pre-push checks (fmt, clippy, test). This blocks `git push` if any check fails.

Build API docs:

```bash
cargo doc -p ownfoil-rs --no-deps --open
```

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
insecure_admin_cookie = false
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

In public mode, admin and settings endpoints are not exposed.

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

- `GET /health` — Returns `{ status: "ok", catalog_files: N }` for readiness checks
- `GET /` (Tinfoil/CyberFoil root payload: `success` + `files`)
- `GET /api/catalog`
- `GET /api/sections`
- `GET /api/sections/:section` where `section in {new,recommended,updates,dlc,all}` (legacy compatibility aliases are also supported)
- `GET /api/shop/sections?limit=<n>` (Ownfoil/CyberFoil-style sections with nested `items`)
- `GET /api/shop/icon/:content_id` (placeholder icon endpoint for client compatibility)
- `GET /api/shop/banner/:content_id` (placeholder banner endpoint for client compatibility)
- `GET /api/search?q=<text>`
- `GET /api/title/:content_id/versions`
- `GET /api/download/*path`
- `GET /api/get_game/:id`
- `GET /api/saves/list` (minimal save-sync compatibility endpoint)

Compatibility aliases:

- `GET /shop`, `GET /index`, `GET /titles`
- `GET /api/shop`, `GET /api/index`, `GET /api/titles`
- `GET /download/*path`

## Admin Web UI

When auth is enabled, an admin web UI is available at `/admin` for browsing the library in a browser.

1. Visit `http://<server-ip>:8465/admin`
2. Log in with credentials from your auth file
3. Browse titles by section (New, Recommended, Updates, DLC, All)
4. Dark theme by default; use the toggle for light theme
5. Log out via the Logout button

The web UI uses session cookies (24h TTL). API requests from the same browser session use the cookie automatically.

## Client Setup (Tinfoil/CyberFoil)

1. Start server and ensure it is reachable from your device.
2. In client custom shop settings, point index/shop URL to:
   - `http://<server-ip>:8465`
3. If auth is enabled, provide matching username/password so the client sends HTTP Basic auth.
4. Verify content downloads resolve to `/api/get_game/:id#filename` and support resume/range requests.

### "Cannot reach shop" troubleshooting

If CyberFoil reports "cannot reach shop", check:

1. **Network reachability** – The Switch must reach the server over the network. From another device on the same LAN (e.g. phone), try:
   ```bash
   curl -v http://<server-ip>:8465/health
   ```
   You should get `{"status":"ok","catalog_files":N}`. If this fails, the Switch cannot reach the server.

2. **Bind address** – The server must listen on all interfaces. Default is `0.0.0.0:8465`. If you use `--bind 127.0.0.1:8465` or a config file with `bind = "127.0.0.1:8465"`, only localhost can connect. Fix: run with `--bind 0.0.0.0:8465` explicitly.

3. **Firewall** – Ensure port 8465 is allowed for incoming connections (e.g. `ufw allow 8465` or equivalent).

4. **Auth** – If the shop is private, CyberFoil needs matching username/password in Settings → Shop. To rule out auth issues, try:
   ```bash
   OWNFOIL_PUBLIC=true cargo run -p ownfoil-rs -- --library-folder ./library
   ```

5. **Shop URL format** – Use `http://<ip>:8465` (no trailing slash). Ensure the IP is your machine’s LAN address (e.g. `192.168.1.x`), not `localhost`.

## Notes

- Filename parsing extracts content identifier/version heuristically (for example, patterns like `[1234567890123456][v123]`).
- This project does not decrypt/encrypt shop payloads; responses are plain JSON.

## Thanks to

- CyberFoil: https://github.com/luketanti/CyberFoil
- Ownfoil (luketanti fork): https://github.com/luketanti/ownfoil
- Ownfoil (original by a1ex4): https://github.com/a1ex4/ownfoil
