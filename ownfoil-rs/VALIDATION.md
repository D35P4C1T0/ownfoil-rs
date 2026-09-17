# Parity validation

Target: Ownfoil v2.4.1, commit `0cce4bbc684b30930b1576847c8c8fb5202114bf`.

## Automated checks

```sh
cargo fmt --all -- --check
cargo test --workspace --locked --offline
cargo clippy --workspace --all-targets --locked --offline -- \
  -D warnings -W clippy::pedantic -W clippy::nursery -W clippy::cargo \
  -A clippy::multiple_crate_versions -D clippy::dbg_macro -D clippy::todo \
  -D clippy::unimplemented -D clippy::print_stdout -D clippy::print_stderr \
  -D clippy::wildcard_imports
```

Regression coverage includes GraphQL authorization, ETags, schema validation,
partial settings updates, task deduplication and child cancellation, refresh retry
scheduling, modern database migration, override-source restoration, archive bounds,
AES-CTR round-trips, and crash recovery with stable IDs across path aliases.

The existing route matrix remains a baseline for the earlier upstream pin. It does
not establish complete compatibility with the new GraphQL surface.

## Optional local checks

These ignored tests require external fixtures and never belong in the default CI
run. Do not commit key files or commercial content.

```sh
# Requires the reference zstd CLI solely as an independent test oracle.
cargo test --locked --offline reference_zstd_interoperability -- --ignored

OWNFOIL_TEST_KEYS=/path/to/prod.keys \
  cargo test --locked --offline local_keys_load_without_disclosing_material -- --ignored

OWNFOIL_TEST_KEYS=/path/to/prod.keys \
OWNFOIL_TEST_ARCHIVE=/path/to/archive.nsp \
OWNFOIL_TEST_MODE=block \
  cargo test --locked --offline local_archive_conversion_roundtrip -- --ignored
```

The archive test copies its input into a temporary library, verifies it, converts
in both directions, and verifies the reconstructed content and stable database ID.
It deletes only its temporary test directory. `OWNFOIL_TEST_MODE` may be `solid` or
`block`; the test uses compression level 1 to keep execution time bounded.

Executed successfully during this update:

- A small real NSP through verification, solid compression, and decompression.
- A larger real NSP through both solid and block conversion round-trips.
- An existing NSZ through decompression and solid recompression.
- Rust/reference Zstandard compatibility in both directions at levels 1 and 18.
- Live HTTP pages, GraphQL mutation, WebSocket snapshots, and task delta events.
- Inline browser-script syntax checks.

XCI/XCZ coverage is synthetic. No browser automation connection or running Docker
daemon was available for visual interaction or image-build checks. See
[PARITY_NOTES.md](PARITY_NOTES.md) for implementation differences still outstanding.

## September 2026 follow-up

The retained partial changes were compared with the pinned upstream `app/tasks.py`,
`app/gql/resolvers.py`, and `app/containers/verification.py`. Added regression
coverage includes scoped file/task lifecycle, nested cancellation, active child
history retention, grouped GraphQL filters and independent roles, malformed NCZ
blocks, decryption probes, streaming hash cancellation, and signature-only
rechecks preserving existing hash verdicts.

Validation commands remain those above. An ARMv6 release build also succeeded for
`arm-unknown-linux-musleabihf` using the installed Rust target, Zig as the C
compiler, and Rust's `rust-lld` linker. `file` and `readelf` identify the result as
an ELF32 ARM EABI5, hard-float, statically linked executable:
`target/arm-unknown-linux-musleabihf/release/ownfoil-rs`.

No ARM hardware or user-mode ARM emulator was available, so this is build
validation, not an ARM runtime test. Docker's socket was absent even outside the
sandbox; image startup was not retested. No browser automation tool was available.
The default suite passed 117 unit/integration tests and 8 route-contract tests
(3 optional tests ignored). The optional `reference_zstd_interoperability` test
was then run separately and passed. Content/key fixture tests remain unrun.
Formatting, strict Clippy, and `git diff --check` also passed.
See [ARM deployment](../docs/ARM.md) for board-side startup and smoke checks.

## September 17 follow-up

- Pinned Python upstream: 141 GraphQL tests passed (`test_gql_graph`,
  `test_gql_catalogue`, `test_gql_app_cards`, `test_gql_etag`, `test_gql_mutations`).
- Rust compares 126 exact GraphQL responses across admin/shop roles against
  `tests/fixtures/graphql_parity.json`. Includes nested hydration, ownership,
  version filters, ordering, pagination, statistics, and task reads.
- NCZ solid/block fixtures cover PFS0/IVFC decryption probes, wrong keys,
  cancellation, declared-size limits, extended-CTR counter buckets, multiple
  buckets, normal-counter metadata tails, and malformed bucket bounds/counts.
- Default suite: 122 passed, 3 optional tests ignored; 8 route tests passed.
  Reference Zstandard interoperability passed separately. Strict Clippy passed.

Regenerate the response fixture using a checkout of
`a1ex4/ownfoil@0cce4bbc684b30930b1576847c8c8fb5202114bf` and a Python environment
with its requirements plus pytest installed:

```sh
python scripts/parity/capture_graphql.py /path/to/pinned-ownfoil ownfoil-rs/tests/fixtures/graphql_parity.json
cargo test --locked graphql_matches_pinned_upstream_response_matrix
```

The synthetic fixture derives from upstream's `tests/test_gql_graph.py` under
AGPL-3.0. It contains no commercial content or console keys. This comparison does
not replace hardware/client checks or the complete golden HTTP matrix.

### CI and runtime follow-up

- GitHub CI and Container workflows succeeded for `b97b005`. Local checks now use
  Rust 1.98.0; Clippy's minimum version matches Cargo's Rust 1.97 requirement.
- Default suite: 124 tests plus 8 route-contract tests passed, 3 optional tests
  ignored. Strict Clippy, formatting, and whitespace checks passed.
- Reference Zstandard interoperability and a real NSZ solid round-trip passed
  separately. The archive was copied into temporary storage; originals were not
  modified.
- Added regressions for heartbeat acknowledgements, task labels with missing
  files or malformed persisted arguments, and conversion recovery preserving
  verification while later external modifications invalidate it.
- Upstream HEAD `84f0b352332cf4ac467db16228b10779f2eb2108` differs from the pinned
  feature baseline only in README documentation.
