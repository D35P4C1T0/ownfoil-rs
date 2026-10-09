# Third-party notices

- `src/http/graphql_contract.json` contains the public schema and descriptions
  extracted from Ownfoil (`a1ex4/ownfoil`, commit
  `0cce4bbc684b30930b1576847c8c8fb5202114bf`). This generated artifact retains
  Ownfoil's AGPL-3.0 license; see `Ownfoil-LICENSE.md`. Regeneration script:
  `../../scripts/parity/extract_schema.py`.
- `src/content/public_moduli.rs` contains public verification constants from
  NSTools (`seiya-dev/NSTools`). See `NSTools-LICENSE.md`.
- Cargo dependencies retain their respective licenses in their source packages.
- `nx-archive/` vendors nx-archive 0.1.2 (MIT), upstream commit
  `9f037ba66c9ad565030d05323ede9a5fc384e318`, with a compatibility patch accepting
  NCA key generations 0x14 through 0x16 found in real-library testing. Its license is
  retained in `nx-archive/LICENSE`. Remove the patch when upstream supports them.
