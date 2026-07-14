# v0.3.0 Validation Record

Date: 2026-07-14

This record separates locally proven behavior from checks that require Switch
hardware, GitHub-hosted builders, or a long-running external TitleDB transfer.

## Passed

- Rust unit, integration, and route-compatibility tests; formatting, strict
  Clippy, and whitespace checks.
- Clean Docker startup as a non-root `PUID`/`PGID`, with settings at
  `/app/config/settings.yaml` and SQLite at `/app/data/ownfoil.db`.
- Admin bootstrap persistence, role assignment, private-shop authentication,
  complete and ranged downloads, HEAD requests, and throttled download counting.
- Watcher create/delete reconciliation, malformed-name fallback, and stable file
  IDs across container recreation.
- Graceful Docker shutdown with exit status 0.
- Compose configuration, isolated startup, health check, shutdown, and cleanup.
- Helm linting and schema validation. A disposable Kubernetes cluster completed
  install, readiness, PVC binding, non-root/read-only-root execution, upgrade,
  rollback, data persistence, and uninstall.
- A read-only `/mnt/iso` library containing 98 files and 185,107,731,036 bytes:
  initial scan, cached restart, 98 unique stable file IDs, and identification of
  67 base applications, 9 updates, and 22 DLC files.
- A 2.34 GB file returned correct HEAD and `bytes 0-1023` range responses.
  Thirty-two concurrent ranges all returned 206. A 64-request health burst was
  rate-limited without restart, and SQLite integrity remained `ok`.
- ARMv6 `arm-unknown-linux-musleabihf` static release cross-compilation produced a
  valid ARM EABI5 hard-float executable without local CPU emulation.
- Live regional TitleDB retrieval parsed 61,798 entries through HTTP byte ranges
  and cached a 17,318,870-byte US English titles file without downloading the full
  1.88 GB ZIP.

## Still External or Partial

- Test current Tinfoil, Aerofoil/CyberFoil, and Sphaira builds on Switch hardware:
  login, public/private shops, encrypted Tinfoil, filters, install, resume, icons,
  banners, and host authorization.
- Run and compare the complete pinned-Python golden HTTP matrix.
- Run the multi-architecture container workflow on GitHub-hosted builders and
  verify the published manifest for amd64, arm64, arm/v7, and arm/v6.
- Allow a complete live TitleDB artifact refresh to finish and verify all cached
  versions, CNMT, and language data.

Report Switch results with the client name/version, URL path, authentication mode,
operation, observed result, and relevant server log lines. Do not include console
keys, passwords, private certificates, or `Hauth` values.
