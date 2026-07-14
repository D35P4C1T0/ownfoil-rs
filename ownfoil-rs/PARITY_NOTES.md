# Parity notes

Target: `a1ex4/ownfoil@7ca28d53f634c6d9cf30786590c966483e819c7b`.

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

Pinned upstream placeholders remain placeholders: `/profile` exposes no save-sync
workflow, and `compress_files` performs no NSP/XCI conversion. Existing NSZ/XCZ
content is supported.
