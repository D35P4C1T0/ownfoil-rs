# Parity notes

Target: `a1ex4/ownfoil@0cce4bbc684b30930b1576847c8c8fb5202114bf`.

Intentional security differences:

- Mutable browser/admin requests require a same-origin `Origin` or `Referer` when
  supplied; redirects are restricted to safe local paths.
- Settings APIs redact Tinfoil's private certificate key as well as all stored
  `hauth` values.
- Organizer and library operations enforce canonical root containment. Destructive
  moves use a recovery journal, and cross-filesystem moves are verified before the
  source is removed.
- Authentication is throttled and cookies are HttpOnly, SameSite, and Secure unless
  the explicit native-development override is enabled.

Resilience differences:

- A container that cannot be decrypted or parsed remains usable through Ownfoil's
  filename convention instead of aborting the scan.
- Watcher events trigger database reconciliation, so duplicate/coalesced events and
  organizer-generated moves converge safely.

Rust-only extensions retained without shadowing upstream routes:

- health, catalog, search, section, and title-version APIs
- TitleDB progress SSE and connectivity probe
- organizer dry-run preview
- legacy Rust route aliases and optional TOML credential import

## Current update

- GraphQL schema, filters, pagination, statistics, task and content mutations.
- Durable SQLite task queue, cancellation, scheduling, worker limits, realtime
  snapshots and add/update/remove events. Interrupted jobs become failed jobs.
- Pure Rust NCZ solid/block encoding and decoding, AES-CTR section transforms,
  RSA-PSS header verification, SHA-256 content verification, and lossless
  container rebuilding. Conversion verifies every reconstructed member before
  publication and records a recovery journal before source removal.
- Compression settings, verification depth, watcher settings, and worker settings.
- Library filters, pagination, file actions, metadata editor, task/statistics pages,
  and local/remote client setup.
- TitleDB release dates and raw metadata, durable overrides, upstream task and
  verification migration, consistent SQLite backups including WAL data.

## Remaining parity work

These are tracked limitations, not completed parity claims:

- Solid compression currently uses one encoder thread. Block compression honors
  the requested thread count, bounded by CPU availability and a 128 MiB batch
  budget. Compression ratios and throughput differ from upstream libzstd.
- Extended/unsupported NCA encryption sections are preserved as encrypted raw
  sections. They round-trip but compress less effectively.
- Verification checks RSA-PSS signatures, signed filesystem headers, CNMT hashes,
  and reconstructed content hashes. Full NSTools decryption-probe equivalence and
  modified/repack classification still need a broader golden fixture matrix.
- Task orchestration currently reconciles libraries for maintenance jobs rather
  than reproducing upstream's complete parent/child task graph.
- Full GraphQL role/filter/relationship compatibility still needs the upstream
  client test matrix. Missing app records persist with stable database IDs.
- Recovery tests cover interrupted publication, damaged-output rejection, and
  stable IDs through canonical path aliases. Real NSP and NSZ conversions pass
  in temporary copies; XCI/XCZ coverage remains synthetic.
- Browser interaction checks could not run because no automation browser was
  connected. Container build checks could not run because Docker was stopped.

`/profile` still exposes no save-sync workflow, matching the prior placeholder.
