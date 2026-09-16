# Ownfoil Feature-Parity Roadmap

Target: [`a1ex4/ownfoil@0cce4bbc684b30930b1576847c8c8fb5202114bf`](https://github.com/a1ex4/ownfoil/commit/0cce4bbc684b30930b1576847c8c8fb5202114bf), v2.4.1.

The prior ledger covered commit `7ca28d53`. The current parity update adds
GraphQL, task workers, realtime events, statistics, metadata overrides, and native
content processing. Full parity is not yet established. See
[parity notes](ownfoil-rs/PARITY_NOTES.md) for remaining differences and validation.

Status: `[x]` implemented and tested, `[~]` implemented but external validation
remains, `[ ]` not implemented.

## Completed Feature Ledger

### Deployment, configuration, and persistence

- [x] Ownfoil paths and files: `/games`, `/app/config/settings.yaml`,
  `/app/config/keys.txt`, `/app/data/ownfoil.db`.
- [x] YAML default merge, validation, obsolete-key cleanup, legacy shop/`hauth`
  migration, and atomic writes.
- [x] Versioned SQLite schema, foreign keys, transactions, stable file IDs,
  many-to-many `app_files`, and upstream database backup/import.
- [x] Durable files, apps, titles, users, roles, identification state, title
  status, download counts, and scan reconciliation.
- [x] `USER_ADMIN_*`/`USER_GUEST_*` bootstrap with first-admin enforcement.
- [x] Docker entrypoint with `PUID`/`PGID`, port 8465, writable state volumes,
  graceful process forwarding, and health check.
- [x] Compose and Helm deployment definitions with game/config/data volumes,
  Service, Ingress/TLS, probes, resources, security context, and image settings.
- [x] Multi-architecture image workflow for amd64, arm64, arm/v7, and arm/v6.

### Authentication and users

- [x] Durable scrypt password hashes, Werkzeug-scrypt compatibility, legacy
  plaintext TOML import, and constant-time verification.
- [x] Independent admin, shop/download, and backup/upload roles; admin bootstrap
  implies all three.
- [x] Browser sessions and HTTP Basic authentication with public/private shop
  behavior across browser, client, and download routes.
- [x] User list/create/delete APIs, validation, duplicate handling, and first
  account setup.
- [x] Safe redirect handling, same-origin mutation protection, secure SameSite
  cookies, login throttling, and role-aware API/page access.
- [x] Settings responses redact every `hauth` and Tinfoil private key.

### TitleDB and content identification

- [x] Ownfoil TitleDB ZIP plus raw-provider fallback; regional titles, versions,
  CNMT, and languages artifacts are parsed and cached.
- [x] Offline last-known-good startup, atomic cache replacement, manual refresh,
  scheduled refresh, overlap guard, progress SSE, and connectivity test.
- [x] Region/language validation and library/cache regeneration.
- [x] Multipart `keys.txt` upload, atomic replacement, malformed-line/revision
  reporting, missing master-key revisions, and secret-safe responses.
- [x] NSP/NSZ/XCI/XCZ CNMT parsing with console keys and resilient filename
  `[APP_ID][vVERSION]` fallback.
- [x] Base/update/DLC mapping, compression state, content count, and multi-content
  relationships persisted with method/error/attempt timestamps.
- [x] Known missing apps, latest-version status, `have_base`, `up_to_date`, and
  `complete` calculation.
- [x] Upstream-shaped `/api/titles` response and deterministic generation cache.

### Library lifecycle and management

- [x] Multiple validated roots and list/add/remove APIs.
- [x] Initial, individual-root, and all-root scans with one global scan lock and
  compatible already-running result.
- [x] Transactional create/change/move/delete reconciliation without duplicate
  rows or lost app ownership.
- [x] Dynamic recursive watcher with stable-copy delay, debounce, root changes,
  and full reconciliation after move/delete or organizer events.
- [x] Runtime scheduler supporting `s`, `m`, `h`, `d`, and `0`.
- [x] Bounded CNMT identification and isolation of malformed containers.
- [x] Per-file/per-host throttled download accounting across range and full-file
  delivery paths.
- [x] Organizer templates for base/update/DLC/multi, all supported variables,
  Unix/Windows sanitization, reserved names, collisions, containment checks,
  verified cross-filesystem moves, empty-directory cleanup, old-update removal,
  dry-run preview, journal, and crash recovery.

### Client protocols and delivery

- [x] Root catch-all dispatch among browser, Tinfoil, CyberFoil, and Sphaira with
  exact header detection and per-client enable flags.
- [x] Base/update/DLC/multi filters, client-specific errors/MOTD, browser fallback,
  and public/private authentication.
- [x] Trusted proxy scheme/host handling, configured-host enforcement, per-host
  `Hauth`, and admin-only first enrollment.
- [x] Tinfoil `Uauth`/`referrer`, exact encrypted `TINFOIL` envelope, Zstandard,
  RSA-OAEP-SHA256, and AES-ECB padding.
- [x] CyberFoil sections, icons, banners, metadata, and unencrypted JSON dispatch.
- [x] Sphaira virtual tree, strict detection, directory-first sorting, filters,
  HTML anchors, and filename lookup.
- [x] Durable-ID downloads, URL fragments, duplicate-safe filename behavior,
  GET/HEAD/OPTIONS, single byte ranges, lengths, and missing-file errors.

### Browser UI and HTTP API

- [x] Login/logout, library/admin, setup, settings, and upstream-placeholder
  profile pages.
- [x] Title cards, ownership/missing/update state, search/section filters, and
  host-aware client setup instructions.
- [x] UI workflows for users, library paths/scans, organizer preview/templates,
  TitleDB, key status/upload, shop/client options, and scheduler.
- [x] Ownfoil settings, users, paths, scan, upload, titles, and durable game-file
  APIs plus non-shadowing Rust compatibility aliases.
- [x] Rust extensions: `/health`, search/catalog APIs, TitleDB progress SSE, and
  organizer preview.

## Intentional Deferred Upstream Placeholders

- [x] `/profile` and `backup_access` retain pinned upstream placeholder behavior;
  real save backup/sync is an extension for a future release.
- [x] `compress_files` remains disabled because the pinned upstream does not
  implement conversion. Existing NSZ/XCZ files are still identified and served.

## v0.3.0 Release Validation

These are verification gates, not missing server features:

- [x] Rust unit/integration/compatibility suite, formatting, strict Clippy, and
  whitespace checks pass locally.
- [~] Capture and compare a full golden HTTP matrix by running the pinned Python
  image against the synthetic fixture library.
- [~] Smoke-test current Tinfoil, Aerofoil/CyberFoil, and Sphaira on Switch
  hardware, including installs, resume, filters, and encrypted shops.
- [x] Clean Docker startup, non-root runtime, persistent config/data placement,
  authentication, range/HEAD downloads, accounting, restart, and graceful stop.
- [x] Compose render/start/health/stop and Helm lint/schema validation plus a real
  Kubernetes install, upgrade, rollback, persistence, and uninstall trial.
- [x] Cross-compile and publish amd64, arm64, arm/v7, and arm/v6 images through
  GitHub Actions, with verified `main`, `latest`, and `v0.3.0` OCI manifests.
- [x] Scan and restart a real 98-file, 185 GB library; verify stable IDs, watcher
  reconciliation, 32 concurrent ranges, rate limiting, SQLite integrity, and
  graceful restart.
- [~] Complete a live refresh of every TitleDB artifact. Regional title retrieval
  through the remote ranged-ZIP reader is proven; unit fixtures cover artifacts,
  but the full live artifact refresh was interrupted.

## Completion Rules

A stable parity claim requires every external validation item above to be `[x]`,
with zero unexplained response diffs and no known auth bypass, secret disclosure,
path traversal, migration corruption, or organizer data-loss issue. The v0.3.0
release is an integration preview while Switch-client, complete golden-matrix,
and full live TitleDB validation remain open.
Intentional security and Rust-extension differences live in
`ownfoil-rs/PARITY_NOTES.md`.
