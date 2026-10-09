# Sphaira parity plan: Ownfoil 2.5.0

Status: implementation in progress on `spharia-support`, 2026-10-09. Four parallel
macro categories cover baseline/validation (1, 7), metadata extraction (3),
artwork storage/background tasks (2, 4), and catalog/download selection (5, 6).
The checkboxes below remain acceptance gates, not claims that hardware tests ran.

Target: [Ownfoil 2.5.0 / a9ac747](https://github.com/a1ex4/ownfoil/tree/a9ac7479f7b54cd52731b24947ac0631cb77ba5f),
with [Sphaira 1.0.8 / 338348e](https://github.com/NaGaa95/sphaira/tree/338348e74b6d278bc570a9be5373d25197bf6c8d).
Pin both revisions in fixtures and comparison tooling so later upstream changes
do not silently change the target.

The goal is equivalent native Sphaira browsing and installation, including the
metadata and artwork pipeline that makes upstream's implementation more complete.
This does not claim parity with every Ownfoil 2.5.0 web-admin feature. Save backup,
dump upload, and resumable upload stay disabled: upstream 2.5.0 does not implement
them either. Existing 2.4.1 parity obligations remain in the root roadmap.

## Existing foundation

Keep the protocol-1 handshake, persistent UID, UDP discovery, public/private shop
access, released client query fixtures, owned-version grouping, durable download
tokens, multiple library roots, Range support, and local image renditions.
See [current compatibility](SPHAIRA.md) for coverage and limitations.

The database already has extracted overrides and app display-version fields,
including upstream import support. Extend these rather than create a parallel
metadata system. At the starting baseline on 2026-10-09, extraction stopped at CNMT content
identification; artwork was fetched on demand; app queries hydrated library
records before pagination. The implementation below replaces those paths.

## 1. Establish the 2.5.0 comparison baseline

- [ ] Extend the existing pinned-upstream response comparisons with Sphaira's
  exact queries, handshake, media fields, and download-copy selection.
- [ ] Build shared fixture scenarios: a title absent from TitleDB; base plus two
  owned updates; a newer unowned update; DLC; a multicontent bundle; compressed
  and uncompressed duplicates; multiple roots; custom metadata; multiple locales.
- [x] Record metadata precedence, locale fallback, same-version tie handling,
  missing-image behavior, rendition encoding, and duplicate-ranking defaults from
  the tagged implementation. Document intentional differences explicitly.
- [ ] Capture a before-change benchmark with identical datasets and queries for
  both servers. Separate cold artwork retrieval from warm catalog responses.

Acceptance: a reproducible gap matrix against the pinned release, with fixtures
that expose each known gap. Existing native contract tests continue to pass.

## 2. Add a persistent media store

Primary areas: `src/http/native.rs`, `src/storage.rs`, a shared media module, and
GraphQL image projection. Paths in this plan are relative to `ownfoil-rs/`.

- [x] Move fetch, decode, rendition, and cache logic into a service usable by HTTP
  handlers, extraction, and background workers.
- [x] Persist title/kind/position slots, source identity, stored filename, and
  original dimensions. Accept both remote artwork and extracted icon bytes.
- [x] Preserve immutable local URLs, atomic writes, bounded downloads/decoding,
  aspect ratio, no upscaling, and authentication on cached media.
- [x] Match upstream ORIGINAL/THUMB/CLIENT/SCREEN semantics, dimensions, JPEG
  quality, and chroma handling; record any deliberate encoding differences.
- [x] Return actual GraphQL image width/height rather than null.
- [x] Migrate or retain existing cache URLs safely; old caches remain rebuildable.

Acceptance: extracted and remote images use the same store; fixtures verify
dimensions, rendition sizes, repeat requests, conditional requests, concurrent
cache misses, and unauthorized requests. A failed write leaves no published
reference to incomplete bytes.

## 3. Extract and merge metadata from game files

Primary areas: `src/identifier.rs`, `src/content/`, `src/storage.rs`,
`src/titledb.rs`, and the `process_file` pipeline in `src/tasks.rs`.

- [x] First audit the archive dependency's Control NCA, RomFS, and NACP support.
  Extend the existing container reader where possible; identify required missing
  primitives before committing to extraction architecture.
- [x] Extract localized title name, publisher, icon, and display version from
  NSP/NSZ/XCI/XCZ, including updates and multicontent containers.
- [x] Share container reads with CNMT identification when both stages are pending.
  Extraction must also run for previously identified files that lack metadata.
- [x] Persist extraction state, locale, source file/app/version, and retryable
  failures. Missing keys and unsupported/corrupt files retain existing fallback
  behavior. Content with no Control NCA must not be retried on every scan.
- [x] Select metadata deterministically from the newest applicable owned content,
  matching the baseline's base/update and tie rules. Older jobs cannot overwrite
  newer extracted metadata.
- [x] Merge fields with upstream precedence: custom override, provider metadata,
  then extracted metadata. Provider regional fallback remains part of provider
  ingestion; missing fields fall through. This order follows upstream
  `SOURCE_PRIORITY = (custom, titledb, extract)`, correcting the original plan.
- [x] Persist per-app display versions, update the title projection, and invalidate
  catalog caches/events before organizer and client views consume the result.
- [x] Requeue extraction when files, keys, or locale change. Reconcile the winning
  metadata when its source file is removed; retain valid older owned sources.
- [x] Add migration/backfill jobs without changing existing file IDs or tokens.

Acceptance: a fixture title absent from TitleDB has its extracted name, publisher,
icon, and display version in Sphaira queries and organizer preview. Test newer vs
older updates, custom precedence, locale fallback, file deletion, missing keys,
malformed metadata, restart, and compressed/bundled containers. Use redistributable
synthetic fixtures; reserve real-container checks for a local owned library.

## 4. Manage artwork in background tasks

Primary areas: `src/tasks.rs`, media service, settings/API/UI, and TitleDB refresh.

- [x] Add upstream-compatible artwork-download configuration and defaults.
- [x] Add all-library and per-title media tasks, including DLC artwork, connected
  to title processing, TitleDB refresh, and locale changes.
- [x] Reuse durable task fan-out, deduplication, cancellation, and progress. Bound
  network concurrency and implement delayed retry for transient failures.
- [x] Keep working artwork during outages or failed refreshes; support extracted
  icons without any external network access.
- [x] Add reference-aware media collection and usage reporting. Match upstream's
  grace period so cleanup cannot remove newly written, not-yet-committed artwork.
  Include custom local references and in-flight writes in retention rules.
- [x] Expose configuration and task status through existing admin surfaces, and
  make the shared image projection available to the web library as well.

Acceptance: after media tasks complete, disconnect external networking and browse
all prepared title icons, banners, and screenshots through local URLs. Test retry,
restart, cancellation, locale replacement, stale references, shared files, and
cleanup racing with ingestion. Never delete library content during media cleanup.

## 5. Match download-copy selection preferences

Primary areas: `src/settings.rs`, `src/http/graph_data.rs`, settings API/UI.
This milestone can ship independently after the baseline.

- [x] Add `library.management.deduplication.prefer_multicontent` with upstream's
  default and configuration/import validation.
- [x] Centralize the best-file comparator: intact before broken, configured
  bundle preference, CNMT identification, verification quality, compression,
  organization, and stable age/ID tie breaking.
- [x] Resolve download URL, size, and extension from the same chosen file.
  Configuration changes must affect selection without changing per-file tokens.

Acceptance: differential fixtures match upstream for both preference values and
mixed verification states. Range requests return the selected file's bytes.
This adds selection parity; automatic duplicate deletion is a separate library
management feature and is not required for this Sphaira milestone.

## 6. Execute Sphaira catalog queries in SQL

Primary areas: `src/http/graphql.rs`, `src/http/graph_data.rs`, storage indexes,
and the merged metadata projection.

- [x] Push filters, ownership, grouping, search, ordering, totals, and pagination
  into parameterized SQL over persisted apps and merged title metadata.
- [x] Hydrate metadata, latest-owned versions, media, and candidate download files
  only for requested fields and returned page IDs; batch relationship reads.
- [x] Preserve current alias handling, nested arguments, stable sort ties, and
  newest-owned selection when a higher version exists only in TitleDB.
- [x] Retain other GraphQL paths until equivalent replacements have comparison
  coverage. Keep administrator-only fields protected in every new query path.
- [ ] Benchmark both implementations on the same small and large fixture sets
  with controlled hardware/cache conditions. Record latency, peak memory, query
  counts, and scaling; set measured budgets before accepting the change.

Acceptance: zero unexplained response differences for the pinned Sphaira matrix;
query tracing shows page-scoped hydration rather than full-library hydration.
Measured scaling improves over our baseline without regressing normal libraries.
Performance superiority over upstream requires a direct benchmark.

## 7. Validate the complete device workflow and release

- [ ] Run workspace tests, formatting, strict CI Clippy, and migration/import
  checks from existing Rust state and the pinned upstream database format.
- [ ] On a physical Switch with Sphaira 1.0.8, verify LAN discovery, saved identity
  after server restart, private authentication, public access, and manual remote
  HTTPS access through the supported reverse proxy configuration.
- [ ] Browse New games, Updates, DLC, All games, and Search; check each sort order,
  multi-page navigation, details, extracted display versions, and local artwork.
- [ ] Install owned base/update/DLC selections from individual and bundled
  NSP/NSZ/XCI/XCZ files, across roots and duplicate preferences. Verify that only
  selected content is installed. Exercise an interrupted/resumed download.
- [ ] Verify prepared offline browsing, locale changes, and refresh while connected.
- [ ] Record client/server versions, library scenarios, results, and remaining
  limitations in `VALIDATION.md`; update `SPHAIRA.md`, roadmap, and changelog.

Acceptance: all automated comparison gates pass and the physical device matrix is
recorded. Missing hardware leaves device validation pending and prevents a full
end-to-end parity claim.

## Implementation evidence and remaining gates

The 2026-10-09 Mac/Switch session confirmed LAN discovery, private login, and
basic browsing. Real-library processing completed all 98 files with zero failures
and 76 extracted metadata records. Key upload/startup scheduling, IVFC layers,
RomFS root metadata/padding, and modern NCA key generations were corrected from
these observations. Full device scenarios, installation, and resume remain open;
see `VALIDATION.md` for the current matrix.

`tests/fixtures/sphaira_250_parity.json` captures 68 responses from the real pinned
Python schema: the 17 released-client query shapes, two pages, and both bundle
preferences. Its synthetic library includes two owned updates, a newer unowned
update, DLC, bundled/compressed duplicates, two roots, sparse custom overrides,
and extracted metadata for a title absent from TitleDB. The fixture records
source hashes, eight regional language mappings, media defaults, and 27 rendition
geometry examples. It supplements the retained 2.4.1 response matrix.

Regeneration: `python scripts/parity/capture_sphaira_250.py /path/to/ownfoil-2.5.0
ownfoil-rs/tests/fixtures/sphaira_250_parity.json` (on one line), with upstream
requirements and pytest installed. Source hash checks reject a different baseline.

`benchmark_sphaira.py` runs the same released catalog/search/sort requests against
prepared servers, verifies identical responses, and records first/warm timings,
response hashes, and optional sampled RSS. Cold artwork requires explicitly
cleared caches. A current-branch Rust scaling smoke benchmark was run for 100 and 10,000 titles
([record](SPHAIRA_SCALING_2026-10-09.json)); hydration stayed at 40 titles/40 apps
and zero file rows for the client card fields. Warm p95 stayed below the measured
local 500 ms budget. No controlled before/after or Python/Rust performance result
has been recorded; comparative timing, true peak memory and traced query-count
acceptance gates remain open.

Upstream tie/locale/media baseline: newest extraction version wins; equal versions
may replace the earlier writer upstream, whereas Rust deliberately chooses the
oldest durable file ID for deterministic outcomes. NACP name and icon each fall
back independently to American English, then the first available language.
Unavailable artwork has null image fields; absent screenshots return null.
Original media bytes and format are preserved; JPEG bounds/qualities are
THUMB 176² icons/320×180 others at90, CLIENT 256²/720×405 at85, SCREEN
720²/1280×720 at85, with4:4:4 chroma and no enlargement. Pillow's Huffman
optimization may produce different bytes than Rust while retaining that contract.
`local_media.enabled=true`, `prefer_multicontent=false`, and a3600-second
collection grace period are the upstream defaults.

The response fixture does not exercise encrypted container decoding, actual
background downloading, locale replacement, or console installation. Those need
the dedicated extraction/media regressions and the device matrix in
[VALIDATION.md](../VALIDATION.md). A physical Switch was unavailable during this
implementation, so end-to-end parity remains unclaimed.

## Delivery order and completion rule

Implement baseline → media store → extraction → background artwork → SQL queries
→ final device validation. Copy-selection preferences can be delivered after the
baseline independently. Check migrations and native contract regressions with
each milestone; perform early Switch smoke tests as milestones become available.

Implemented checkboxes describe shipped code paths; acceptance paragraphs remain
subject to the recorded automated/device evidence and open gates.

Claim **Sphaira feature parity with Ownfoil 2.5.0** only after metadata, artwork,
selection, protocol comparisons, and device acceptance criteria are satisfied.
Track broader Ownfoil admin/UI parity and existing external validation separately.

Reference implementation:
[handshake](https://github.com/a1ex4/ownfoil/blob/2.5.0/app/app.py),
[metadata and media tasks](https://github.com/a1ex4/ownfoil/blob/2.5.0/app/tasks.py),
[NACP](https://github.com/a1ex4/ownfoil/blob/2.5.0/app/containers/nacp.py),
[media store](https://github.com/a1ex4/ownfoil/blob/2.5.0/app/media.py),
[GraphQL resolvers](https://github.com/a1ex4/ownfoil/blob/2.5.0/app/gql/resolvers.py).
