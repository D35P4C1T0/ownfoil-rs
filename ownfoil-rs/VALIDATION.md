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
