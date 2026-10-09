# Sphaira native Ownfoil compatibility

The fork previously supported Sphaira's HTTP directory browser. Sphaira 1.0.8
adds a separate native Ownfoil menu. This implementation targets API protocol 1,
using these inspected upstream revisions:

- [Ownfoil a9ac747](https://github.com/a1ex4/ownfoil/tree/a9ac7479f7b54cd52731b24947ac0631cb77ba5f)
- [Sphaira 1.0.8 / 338348e](https://github.com/NaGaa95/sphaira/tree/338348e74b6d278bc570a9be5373d25197bf6c8d)

## Connection and discovery

`OPTIONS /` returns the persisted server UID, shop name, server version,
`protocol_version: 1`, MOTD, public status, remote address, and feature flags.
Shop browsing and ranged downloads are available. Save backup, dump upload, and
resumable upload flags remain false because those protocols are not implemented.
The handshake is independent of legacy client detection and enablement toggles.

The UDP responder listens on port 8465 and replies to `OWNFOIL_DISCOVER` with
`magic: OWNFOIL`, the same UID, shop name, version, actual HTTP port, remote
address, and public status. It tracks changes to shop settings and
`services.discovery.enabled` without restarting. A busy UDP port does not prevent
HTTP startup. The UID is stored in `server.uid` in settings.yaml and survives
restarts. Preserve that settings file to retain saved client identity.

For LAN HTTP bindings, discovery binds to `0.0.0.0:8465` so it can receive
IPv4 broadcasts even when HTTP binds to a specific LAN address. Loopback HTTP
bindings keep discovery on `127.0.0.1`. Sphaira sends to its subnet's broadcast
address and `255.255.255.255`, retries every 500 ms, and collects replies for
1.5 seconds. It uses the reply's source IP and advertised HTTP port to connect.

Discovery broadcasts need to reach the server directly: on Linux Docker, use
`compose.discovery.yaml` with the base Compose file. Allow UDP 8465 and the HTTP
port on the LAN. On Docker Desktop, across VLANs, or through reverse proxies,
manual address configuration remains available. HTTPS remote connections use the
existing reverse proxy configuration.

## Catalog and installation

Game names and artwork require populated TitleDB metadata. The default source
is `https://github.com/a1ex4/ownfoil/releases/download/titledb`, whose
`titles.<region>.<language>.json.zst` assets include regional fallback titles.
When configuring a `[titledb]` section explicitly, set `enabled = true` and
`url_override` to that release base URL to use the consolidated metadata.
Legacy ZIP source overrides remain supported. Icons are downloaded and cached
on first access; reconnect the client after refreshing metadata.

App-only GraphQL requests load metadata for the library's app and parent title
IDs through indexed SQLite lookups. Global title searches and statistics retain
the complete catalog. On the local dev server with 44 base games and 63,323
TitleDB entries, Sphaira's first-page query dropped from 19.4–19.7 seconds to
0.10–0.14 seconds. These timings cover the API response; initial artwork downloads
and console rendering take additional time.

The GraphQL API accepts the released client's catalog, search, update pre-pass,
and aliased title-details queries. This includes `StringFilter.notIn`, image
size arguments, app display versions, latest owned versions, added-time sorting,
and download URL/extension/size fields. Display versions are nullable; the new
column stores supplied/imported metadata but this change does not add Control
NCA/NACP extraction. Existing CNMT identification continues to determine the
contents carried by each file.

Grouped `owned:true` queries return the newest **owned** version. An unavailable
newer version in TitleDB cannot replace its installation link. Nested base,
update, and DLC selections resolve their own fields and artwork sizes separately.
The client chooses content IDs within bundled containers; the server streams the
selected container, including NSP, NSZ, XCI, and XCZ, through existing Range support.

Download links use random, durable per-file tokens and still require shop access.
Tokens survive rescans, organization, and restarts. Links follow each file's own
library root. When copies exist, selection prefers intact verification results,
single-content files, CNMT identification, better verification, compression,
organization, then stable added-time/id ordering. Legacy path and numeric download
endpoints remain available for existing clients.

Public shops allow anonymous GraphQL catalog queries. Their anonymous callers
cannot access admin file paths, tasks, settings, or mutations. Private shops
require authenticated shop access; existing administrator-only permissions remain.

## Artwork

The API returns local `Image` URLs for icons, banners, and screenshots. The server
fetches artwork on demand, caches source bytes, and creates JPEG renditions without
enlarging small originals. THUMB, CLIENT, and SCREEN use the upstream bounding
boxes, preserving aspect ratios. Source downloads and image decoding are bounded.
Cached artwork supports conditional requests and requires shop access even when
already stored. GraphQL image width/height remain null because dimensions are not
recorded in the catalog. Upstream's background artwork ingestion, extracted icons,
and media collection are outside this client integration change.

## Verification

`ownfoil-rs/tests/fixtures/sphaira_native.json` records the released client's exact
fields and query shapes for all five catalog categories, three sort orders,
update discovery, and title details. Tests cover owned-version selection, aliased
metadata, bundled content, multiple library roots, stable tokens, ranged downloads,
public/private access, real UDP discovery, and artwork fetching/resizing/caching
through a local HTTP fixture. Older GraphQL parity expectations are retained,
with an explicit adjustment for the new owned-grouping behavior.

Run the workspace tests and strict CI Clippy command with Rust 1.97 or newer.
Physical Switch testing remains required to confirm the complete device workflow;
these tests validate the server protocol and do not perform a console installation.

Validated on 2026-10-09 with Rust 1.98.0: **138 tests passed**, with three existing
optional archive/zstd tests ignored. Strict CI Clippy, formatting, diff whitespace
checks, and the merged Compose discovery configuration passed.
