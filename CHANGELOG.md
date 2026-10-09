# Changelog

## Unreleased

- Extract localized name, publisher, icon, and display version from Control
  NCA/NACP metadata, including bounded compressed-member decoding. Persist
  extraction provenance and select the newest available owned source while
  retaining custom and TitleDB metadata precedence.
- Persist artwork slots and dimensions, preserve image formats, and add
  background artwork downloads, offline serving, usage reporting, and safe
  collection of unreferenced media.
- Add configurable preference for bundled download copies and execute native
  app catalog filters, grouping, sorting, and pagination in SQLite with scoped
  relationship hydration.
- Add responses captured from the pinned Ownfoil 2.5.0 schema and controlled
  benchmark tooling. New end-to-end Switch installation validation remains open.
- Start extraction after valid key upload and at startup. Fix IVFC layer counts,
  root RomFS metadata selection/padding, and newer NCA key generations found in
  real-library testing. All 98 files processed without failure, yielding 76
  extracted metadata records; Switch discovery, private login, and browsing
  confirmed. Installation and resume remain pending.
- Validate 162 automated tests, including 68 exact upstream response comparisons;
  four optional tests are ignored in the normal suite, and the scaling benchmark
  passes separately. Strict CI Clippy, formatting, and whitespace checks pass.

## v0.4.0 - 2026-10-09

### Sphaira support

- Add Sphaira 1.0.8 native Ownfoil API support: connection handshake, durable
  identity, configurable UDP LAN discovery, catalog and title queries, owned
  version selection, and token downloads across multiple libraries.
- Serve cached artwork with console-sized renditions and enable anonymous
  GraphQL shop browsing for public shops while preserving admin permissions.
- Read consolidated TitleDB metadata from upstream's Zstandard-compressed
  release assets, including regional titles absent from the US eShop catalog.
- Load app-list metadata through indexed title lookups instead of rebuilding
  the entire global TitleDB for each Sphaira shop page. On the tested library
  with 44 games and 63,323 metadata entries, the shop query dropped from about
  19.5 seconds to 0.10 seconds.
- Add native client contract tests, updated setup instructions, and a Linux
  Compose host-network override for discovery.

### Validation

- Verified LAN discovery, connection, names, icons, and shop browsing with
  Sphaira on a physical Switch. Console installation remains unverified.
- 138 automated tests pass, with three existing optional tests ignored; strict
  CI Clippy, formatting, and Compose discovery configuration checks pass.

## v0.3.0 - 2026-07-14

This integration-preview release ports the pinned Ownfoil feature set to Rust.

### Added

- Durable SQLite libraries, catalog state, users, roles, identification results,
  download accounting, and migration support.
- Ownfoil-compatible settings, TitleDB metadata, console-key content
  identification, completeness tracking, multi-library scanning, watching,
  scheduling, and safe organization workflows.
- Tinfoil encryption and host authorization, CyberFoil responses, Sphaira virtual
  directories, browser administration, uploads, and resumable downloads.
- Production Docker/Compose deployment, Helm chart, health checks, non-root
  runtime, graceful shutdown, and multi-architecture publication workflow.

### Fixed

- Store the container database under `/app/data` rather than `/app/config`.
- Preserve stable IDs and reconcile watcher/database state after file changes.
- Avoid full in-memory TitleDB ZIP downloads by reading remote ZIP members through
  HTTP ranges.
- Forward optional Compose bootstrap variables only when supplied, and support a
  configurable read-only or read-write games mount.

### Validation

See [VALIDATION.md](VALIDATION.md). Switch-client testing, the complete upstream
golden matrix, and a complete live TitleDB artifact refresh remain external
validation gates.
