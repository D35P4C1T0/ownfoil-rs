# PARITY_NOTES

## Implemented

- Core library scanner for `.nsp/.xci/.nsz/.xcz` files
- Catalog/index endpoint returning title metadata and download URLs
- Section browsing with Ownfoil/CyberFoil-style groups (`new/recommended/updates/dlc/all`) and aliases
- Tinfoil-style root payload (`/`) with `files: [{url,size}]`
- Ownfoil-style game stream endpoint (`/api/get_game/:id`)
- CyberFoil-compatible media endpoints (`/api/shop/icon/:title_id`, `/api/shop/banner/:title_id`)
- Search endpoint over filename and title id
- Title version listing by `title_id`
- File download endpoint with byte range support (`Range`, `Content-Range`, `Accept-Ranges`)
- Minimal save-sync list endpoint (`/api/saves/list`) returning empty list when no backups are available
- Optional HTTP Basic auth via:
  - `Authorization: Basic <base64(username:password)>`
- Config-based auth via separate credentials file (`auth_file` with `username`/`password` and optional `[[users]]`)
- Private-by-default startup policy (auth file required unless `OWNFOIL_PUBLIC=true`)
- Public mode disables admin/settings routes
- Admin session cookie is `Secure` by default (override via `OWNFOIL_INSECURE_ADMIN_COOKIE=true`)
- Endpoint aliases for compatibility (`/shop`, `/index`, `/titles`, and `/api/*` variants)
- Shop sections now mirror Ownfoil/CyberFoil behavior by deduplicating updates and DLC to latest version per content id

## Intentionally Removed

- Web UI/templates/static assets
- NSZ conversion/compression/decompression workflows
- Non-essential administrative endpoints and background jobs unrelated to core game-serving

## Known Deviations

- Metadata extraction is filename-based heuristics only (no NCA/NSP deep metadata parsing).
- Range handling supports single ranges; multi-range requests are rejected.
- JSON response schema is compatibility-oriented, not a full reimplementation of every Python route shape.
