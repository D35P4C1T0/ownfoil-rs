# Drop-In Replacement Plan

Target: make `ownfoil-rs` behave like `a1ex4/ownfoil` for existing deployments and clients, while keeping the Rust implementation small and testable.

## Current Parity Snapshot

Implemented:

- Core content serving for `.nsp`, `.xci`, `.nsz`, and `.xcz` files
- Recursive scanning for a single library root
- Filename/path-based title ID and version parsing
- Basic catalog, search, sections, title versions, download-by-path, and download-by-file-ID endpoints
- CyberFoil-style `/api/shop/sections` and `/api/get_game/:id`
- Tinfoil-style encrypted shop payload support
- Placeholder icon/banner endpoints, with TitleDB redirects when metadata is present
- Basic-auth credentials from TOML
- Minimal admin pages for browsing and settings
- Partial-content download support through `Range`

Missing for true Ownfoil drop-in behavior:

- Root-path client dispatch equivalent to upstream `GET /` and `GET /<path:path>`
- Browser pages and flows: `/`, `/settings`, `/setup`, `/profile`, `/login`, `/logout`
- Upstream API routes:
  - `GET /api/settings`
  - `POST /api/settings/titles`
  - `POST /api/settings/shop`
  - `GET|POST|DELETE /api/settings/library/paths`
  - `POST /api/settings/library/management`
  - `POST /api/settings/scheduler`
  - `POST /api/upload`
  - `GET /api/titles`
  - `GET /api/get_game/<int:id>`
  - `POST /api/library/scan`
  - `GET /api/users`
  - `DELETE /api/user`
  - `POST /api/user/signup`
- Persistent SQLite-like state for libraries, files, titles, apps, users, download counts, and identification status
- Multi-user authentication with hashed passwords and `admin`, `shop`, and `backup` permissions
- First-admin creation flow and `USER_ADMIN_*` / `USER_GUEST_*` environment bootstrap
- Ownfoil YAML settings compatibility under `/app/config/settings.yaml`
- Docker/runtime compatibility for `/games`, `/app/config`, `/app/data`, `PUID`, and `PGID`
- Multiple library paths
- File watcher integration for create, modify, move, and delete events
- Manual scan endpoint and scan-in-progress locking
- TitleDB language/region settings and automatic TitleDB update scheduling
- Console `keys.txt` upload and validation
- Content identification from CNMT/decryption, with filename fallback
- Missing update, DLC, and base completeness tracking
- Library cache hash and unchanged-library fast path
- Automatic organization: templates, file moves, empty-folder cleanup, and old-update deletion
- Sphaira directory-browsing mode, including virtual folders and file links under filtered paths
- Tinfoil-specific request handling: client header identification, `Hauth`/`Uauth`, host verification, shop customization, and URL-based content filtering
- CyberFoil-specific request handling: client header identification, host verification, MOTD/info/error responses
- Accurate `/api/titles` response shape: `{ total, games }` with upstream title/app fields
- Full real-media parity for icons and banners from TitleDB
- Activity/download accounting compatible with upstream `download_count`

## Phase 0 - Compatibility Harness

- Freeze upstream behavior with fixtures:
  - Clone a pinned upstream commit in test docs.
  - Capture sample JSON/HTML responses for browser, Tinfoil, CyberFoil, and Sphaira.
  - Add Rust golden tests for headers, status codes, content type, auth failures, and response shapes.
- Build a route matrix in tests where every upstream route is implemented, stubbed with a compatible shape, or explicitly unsupported.
- Add a fixture library with base, update, DLC, XCI, NSZ, nested folders, unknown title, and bad filename cases.

Exit criteria:

- CI reports current parity gaps as failing/ignored tests, not tribal knowledge.

## Phase 1 - API And Deployment Drop-In

- Match upstream paths and defaults:
  - Serve the browser UI at `/`, not only `/admin`.
  - Add `/settings`, `/setup`, `/profile`, `/login`, and `/logout`.
  - Add all upstream settings, user, upload, and library scan endpoints with compatible JSON shapes.
  - Keep existing aliases as backwards-compatible extras.
- Add config compatibility:
  - Read/write `/app/config/settings.yaml`.
  - Keep current TOML/CLI as a Rust-specific compatibility layer.
  - Support `/games`, `/app/config`, and `/app/data` defaults.
  - Support `USER_ADMIN_NAME`, `USER_ADMIN_PASSWORD`, `USER_GUEST_NAME`, and `USER_GUEST_PASSWORD`.
- Add persistent storage:
  - Introduce `Libraries`, `Files`, `Titles`, `Apps`, and `Users` tables with fields matching upstream semantics.
  - Migrate the in-memory catalog to DB-backed catalog snapshots.
  - Track download counts and identification errors.

Exit criteria:

- Existing Ownfoil Docker volume layouts can boot `ownfoil-rs` without manual config conversion.
- Browser, settings, and user routes return upstream-compatible shapes.

## Phase 2 - Client Protocol Parity

- Implement upstream root dispatch:
  - Identify Tinfoil, CyberFoil, Sphaira, or browser requests from headers/path.
  - Apply per-client enable flags.
  - Return per-client error/info formats.
- Tinfoil:
  - Match URL filtering for games, updates, DLC, and XCI.
  - Match encrypted and unencrypted shop payload fields.
  - Implement `Hauth`/`Uauth` host verification and admin-assisted first host registration.
- CyberFoil:
  - Match sections, icons, banners, MOTD, auth, and host verification behavior.
- Sphaira:
  - Implement directory-style HTML listing.
  - Support virtual filtered directories and direct file downloads.
  - Support HEAD/OPTIONS behavior expected by Sphaira.

Exit criteria:

- Tinfoil, CyberFoil, and Sphaira can use the Rust server with the same URL/user setup as upstream.

## Phase 3 - Metadata And Identification

- Add TitleDB parity:
  - Download/update `cnmts.json`, `versions.json`, `versions.txt`, and `languages.json`.
  - Store the selected region/language.
  - Serve real icon/banner metadata.
- Add console key workflows:
  - Upload `keys.txt`.
  - Validate missing/corrupt master keys.
  - Persist key validation status without leaking keys.
- Add content identification:
  - Keep filename fallback.
  - Add a CNMT/decryption path that identifies app ID, title ID, version, app type, multi-content files, and compression.
  - Persist attempts, errors, and identification type.
- Compute completeness:
  - `have_base`, `up_to_date`, and `complete`
  - Missing update and DLC lists from TitleDB/version data

Exit criteria:

- The Rust catalog matches upstream generated library output for representative NSP/XCI/NSZ/XCZ fixtures.

## Phase 4 - Library Management

- Add multiple library paths with add/remove APIs.
- Add a file watcher for create, modify, move, and delete events.
- Add manual scan and scheduled scan/update with lock semantics.
- Add a library cache and unchanged-library fast path.
- Add organizer behavior:
  - Template-based destination paths
  - Move/rename files
  - Remove empty folders
  - Delete older updates
- Add optional compression/conversion workflows only after core parity is stable.

Exit criteria:

- Moving, deleting, adding, and renaming files updates DB and shop output without a restart.

## Phase 5 - Web UI Parity

- Replace the minimal admin UI with Ownfoil-compatible pages:
  - Library browse
  - Settings
  - Setup instructions
  - Profile/backups placeholder if backup parity is not yet implemented
  - User management
  - Title settings and keys upload
  - Library paths and organizer settings
- Keep the UI server-rendered unless a real need for frontend complexity appears.

Exit criteria:

- Common Ownfoil admin workflows can be completed from the browser without touching config files.

## Selected Priority Order

User-selected order:

1. TitleDB and real metadata
2. Compatibility harness and route tests
3. Persistent DB schema plus import from the current scanner
4. Upstream-compatible auth/users/settings routes
5. Root dispatch and Sphaira/Tinfoil/CyberFoil protocol parity

Root dispatch and full client protocol parity can wait until after metadata, tests, persistence, and admin API work.
