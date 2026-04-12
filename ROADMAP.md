# Roadmap

Planned work toward making `ownfoil-rs` a closer AeroFoil drop-in replacement.

## Core Parity

- [ ] Match AeroFoil root shop behavior at `/`, including client-aware Tinfoil/CyberFoil response handling
- [x] Support encrypted Tinfoil payloads for `/` and `/api/shop/sections`
- [ ] Add AeroFoil-compatible `/api/titles` response shape with `games`, pagination, filters, and discovery sections
- [ ] Match AeroFoil discovery ordering for `new` and `recommended`
- [ ] Preserve stable file IDs and route semantics compatible with AeroFoil clients
- [ ] Add request/response compatibility tests against upstream AeroFoil fixtures and behavior

## Save Sync

- [ ] Implement real `GET /api/saves/list` backed by on-disk save data
- [ ] Add `POST /api/saves/upload/<title_id>`
- [ ] Add `GET /api/saves/download/<title_id>/<save_id>`
- [ ] Add `DELETE` and `POST` compatibility for `/api/saves/delete/<title_id>/<save_id>`
- [ ] Support multiple save versions per title with metadata such as note, created time, and size
- [ ] Enforce AeroFoil-style save-sync authorization rules

## Auth And Users

- [ ] Expand auth model beyond flat TOML credentials to support AeroFoil-style users and permissions
- [ ] Support separate admin, shop, and backup access flags
- [ ] Add frozen account behavior and messaging
- [ ] Add auth lockout and IP blacklist protections
- [ ] Add user management endpoints compatible with AeroFoil admin flows

## Library And Metadata

- [ ] Improve content identification beyond filename heuristics
- [ ] Support keys-based metadata extraction workflows
- [ ] Track base, update, and DLC completeness the way AeroFoil does
- [ ] Enrich titles with TitleDB description, screenshots, genre, and display-name metadata
- [ ] Add title detail endpoints such as `/api/title-details` and `/api/title-info/<title_id>`

## Media

- [ ] Cache icon and banner assets locally instead of redirecting or serving placeholders only
- [ ] Generate resized icon and banner variants similar to AeroFoil
- [ ] Add media cache refresh and prefetch operations

## Admin And Web UI

- [ ] Expand the admin UI beyond basic browsing to cover settings and management flows
- [ ] Add pages and APIs for settings, users, saves, activity, requests, uploads, and downloads
- [ ] Mirror AeroFoil browser-facing behavior for public shop vs authenticated admin access

## Library Management

- [ ] Add manual library scan endpoints
- [ ] Add support for multiple library paths
- [ ] Add background file watching for library changes
- [ ] Add library management operations such as organize, delete older updates, delete duplicates, and orphan cleanup

## Downloads And Automation

- [ ] Add missing update search and download automation
- [ ] Add Prowlarr integration
- [ ] Add torrent and usenet client integrations
- [ ] Add download queue, active transfer, and completed download endpoints

## Conversion

- [ ] Add NSP and XCI to NSZ conversion workflows
- [ ] Add conversion job tracking and cancellation APIs
- [ ] Support optional staging directories for conversion output

## Observability And Ops

- [ ] Add AeroFoil-like activity and access history tracking
- [ ] Track download counts and transfer accounting compatible with AeroFoil semantics
- [ ] Add diagnostics and health endpoints for admin tooling
- [ ] Support more AeroFoil-compatible environment variables and deployment conventions

## Quality

- [ ] Build a route-by-route parity matrix against upstream AeroFoil
- [ ] Add regression tests for Tinfoil, CyberFoil, browser, and admin use cases
- [ ] Add fixture-based compatibility tests using representative AeroFoil responses
- [ ] Document supported parity level clearly for each endpoint
