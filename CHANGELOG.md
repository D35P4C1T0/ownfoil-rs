# Changelog

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
golden matrix, hosted multi-architecture publication, and a complete live TitleDB
artifact refresh remain external validation gates.
