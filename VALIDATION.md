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
- GitHub Actions cross-compiled and published verified amd64, arm64, arm/v7, and
  arm/v6 OCI manifests for `main`, `latest`, and `v0.3.0`. The ARMv6 build also
  produced a valid ARM EABI5 hard-float executable locally without CPU emulation.
- GitHub Actions built and attached Linux x86-64, macOS ARM64, and Windows x86-64
  archives to the v0.3.0 release.
- Live regional TitleDB retrieval parsed 61,798 entries through HTTP byte ranges
  and cached a 17,318,870-byte US English titles file without downloading the full
  1.88 GB ZIP.

## Still External or Partial

- Test current Tinfoil, Aerofoil/CyberFoil, and Sphaira builds on Switch hardware:
  login, public/private shops, encrypted Tinfoil, filters, install, resume, icons,
  banners, and host authorization.
- Run and compare the complete pinned-Python golden HTTP matrix.
- Allow a complete live TitleDB artifact refresh to finish and verify all cached
  versions, CNMT, and language data.

Report Switch results with the client name/version, URL path, authentication mode,
operation, observed result, and relevant server log lines. Do not include console
keys, passwords, private certificates, or `Hauth` values.

## Sphaira / Ownfoil 2.5.0 parity implementation — 2026-10-09

Server reference: Ownfoil `a9ac7479f7b54cd52731b24947ac0631cb77ba5f` (2.5.0).
Client reference: Sphaira `338348e74b6d278bc570a9be5373d25197bf6c8d` (1.0.8).
Rust checks use toolchain 1.98.0. The earlier 2.4.1 validation record is retained.

The pinned Python schema generated 68 released-client response fixtures without
keys, commercial content, or live TitleDB downloads. Scenarios include missing
provider metadata filled by extraction, sparse custom/provider/extract precedence,
two owned updates and a newer unowned update, DLC, bundles, compressed duplicates,
multiple roots, pagination, search and sort orders. Eight locale mappings and
27 rendition geometry examples are also captured. Regeneration rejects changed
source hashes. Dedicated Rust contract tests replay every response exactly.

A controlled HTTP benchmark runner is available in
`scripts/parity/benchmark_sphaira.py`. Both servers must receive identical seeded
content/metadata; it refuses inconsistent responses. No completed before/after
latency, true peak-memory or traced-query-count comparison is recorded. Existing
latency figures remain Rust-only observations and do not prove superiority.

Prior native integration discovery and browsing were verified on a physical
Switch, as recorded in CHANGELOG.md; console installation remained unverified.
On 2026-10-09, the user confirmed LAN discovery, private-shop login, and game
browsing on a physical Switch against this Mac's release build on
`spharia-support`. The installed Sphaira version and remaining scenarios have
not yet been recorded.

| Device check (Sphaira 1.0.8) | Required scenarios | Result |
| --- | --- | --- |
| Discovery and identity | LAN, manual HTTPS, restart, saved shop UID | LAN discovery passed; remaining scenarios pending |
| Authentication | Private valid/invalid login, public shop, cached artwork | Private valid login passed; remaining scenarios pending |
| Browsing | New/Updates/DLC/All/Search, each sort, multiple pages, details | Basic game browsing passed; complete matrix pending |
| Metadata | NACP name/publisher/icon/version, absent TitleDB, locale switch | Pending |
| Installation | Base/update/DLC, NSP/NSZ/XCI/XCZ, bundles, both preferences, roots | Pending |
| Resume | Interrupt and resume an installation, confirm selected bytes/content | Pending |
| Prepared offline browsing | Download artwork, disconnect external network, browse/refresh | Pending |

Record device/server builds, authentication mode, scenario, result and relevant
logs before claiming end-to-end parity. The isolated Mac test instance processed
98 real library files with uploaded keys: 98 completed, zero failed, and 76
extracted metadata records. This exposed and verified fixes for IVFC layer counts,
RomFS nested names/padding, newer NCA key generations, and automatic extraction
after key upload/startup. Complete processing can also mean a file has no Control
metadata. Archive installation and resume remain pending. Never commit keys,
passwords, or commercial archives.

Automated parity run: **154 unit/integration tests plus eight route-contract
tests passed (162 total), four optional tests ignored**. The separately invoked
ignored scaling benchmark also passed, using Rust 1.98.0 on macOS aarch64, debug
build, sequential requests through the in-process Axum test transport. The
current implementation recorded these warm response times (20 samples per
query; library seeding and first provider synchronization excluded):

| Synthetic base titles | Sort | p50 ms | p95 ms |
| ---: | --- | ---: | ---: |
| 100 | Name | 37.3 | 77.4 |
| 100 | Added time | 32.8 | 37.3 |
| 100 | Release date | 33.1 | 34.2 |
| 10,000 | Name | 160.0 | 161.6 |
| 10,000 | Added time | 155.0 | 199.1 |
| 10,000 | Release date | 154.3 | 156.0 |

A 500 ms warm p95 budget is enforced by this optional local benchmark, with
`OWNFOIL_BENCHMARK_P95_BUDGET_US` available for explicitly chosen hardware budgets.
Both sizes hydrated exactly 40 titles/40 apps/zero file rows for the card fields;
a normal regression test separately verifies seven titles/apps/files for a
seven-item page requesting file relationships. Other parallel development checks
ran on this host, so these are scaling smoke measurements, not a controlled
Python/Rust or before/after comparison. Raw measurements and first-provider
synchronization timings: [JSON record](docs/SPHAIRA_SCALING_2026-10-09.json).
