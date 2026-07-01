use std::collections::BTreeSet;

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ParityState {
    Supported,
    StubCompatible,
    Gap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Route {
    method: &'static str,
    path: &'static str,
    state: ParityState,
}

const UPSTREAM_ROUTE_MATRIX: &[Route] = &[
    Route { method: "GET", path: "/", state: ParityState::Supported },
    Route { method: "GET", path: "/api/catalog", state: ParityState::Supported },
    Route { method: "GET", path: "/api/sections", state: ParityState::Supported },
    Route { method: "GET", path: "/api/shop/sections", state: ParityState::Supported },
    Route { method: "GET", path: "/api/get_game/:id", state: ParityState::Supported },
    Route { method: "GET", path: "/api/shop/icon/:title_id", state: ParityState::StubCompatible },
    Route { method: "GET", path: "/api/shop/banner/:title_id", state: ParityState::StubCompatible },
    Route { method: "GET", path: "/api/saves/list", state: ParityState::StubCompatible },
    Route { method: "GET", path: "/api/titles", state: ParityState::Gap },
    Route { method: "GET", path: "/settings", state: ParityState::Gap },
    Route { method: "GET", path: "/setup", state: ParityState::Gap },
    Route { method: "GET", path: "/profile", state: ParityState::Gap },
    Route { method: "GET", path: "/login", state: ParityState::Gap },
    Route { method: "GET", path: "/logout", state: ParityState::Gap },
    Route { method: "GET", path: "/api/settings", state: ParityState::Gap },
    Route { method: "POST", path: "/api/settings/titles", state: ParityState::Gap },
    Route { method: "POST", path: "/api/settings/shop", state: ParityState::Gap },
    Route { method: "GET", path: "/api/settings/library/paths", state: ParityState::Gap },
    Route { method: "POST", path: "/api/settings/library/paths", state: ParityState::Gap },
    Route { method: "DELETE", path: "/api/settings/library/paths", state: ParityState::Gap },
    Route { method: "POST", path: "/api/settings/library/management", state: ParityState::Gap },
    Route { method: "POST", path: "/api/settings/scheduler", state: ParityState::Gap },
    Route { method: "POST", path: "/api/upload", state: ParityState::Gap },
    Route { method: "POST", path: "/api/library/scan", state: ParityState::Gap },
    Route { method: "GET", path: "/api/users", state: ParityState::Gap },
    Route { method: "DELETE", path: "/api/user", state: ParityState::Gap },
    Route { method: "POST", path: "/api/user/signup", state: ParityState::Gap },
];

#[test]
fn upstream_ownfoil_route_matrix_has_no_duplicate_entries() {
    let mut seen = BTreeSet::new();

    for route in UPSTREAM_ROUTE_MATRIX {
        assert!(
            seen.insert((route.method, route.path)),
            "duplicate compatibility route entry: {} {}",
            route.method,
            route.path
        );
    }
}

#[test]
fn current_supported_and_stub_compatible_routes_are_explicit() {
    let implemented: BTreeSet<_> = UPSTREAM_ROUTE_MATRIX
        .iter()
        .filter(|route| matches!(route.state, ParityState::Supported | ParityState::StubCompatible))
        .map(|route| (route.method, route.path))
        .collect();

    for expected in [
        ("GET", "/"),
        ("GET", "/api/catalog"),
        ("GET", "/api/shop/sections"),
        ("GET", "/api/get_game/:id"),
        ("GET", "/api/shop/icon/:title_id"),
        ("GET", "/api/shop/banner/:title_id"),
        ("GET", "/api/saves/list"),
    ] {
        assert!(
            implemented.contains(&expected),
            "missing current compatibility entry: {} {}",
            expected.0,
            expected.1
        );
    }
}

#[test]
fn upstream_gap_routes_are_visible_and_non_empty() {
    let gaps: Vec<_> =
        UPSTREAM_ROUTE_MATRIX.iter().filter(|route| route.state == ParityState::Gap).collect();

    assert!(
        gaps.len() >= 10,
        "expected browser, settings, users, upload, and /api/titles gaps to stay visible"
    );
    assert!(gaps.iter().any(|route| route.path == "/api/titles"));
    assert!(gaps.iter().any(|route| route.path == "/settings"));
    assert!(gaps.iter().any(|route| route.path == "/api/users"));
}

#[test]
fn tinfoil_root_response_shape_fixture_matches_current_surface() {
    let body = json!({
        "success": "Welcome to ownfoil-rs",
        "files": [
            {
                "url": "/api/get_game/1#Demo.nsp",
                "size": 4096
            }
        ]
    });

    assert!(body["success"].is_string());
    let files = body["files"].as_array().expect("files array");
    assert_eq!(files.len(), 1);
    assert!(files[0]["url"].as_str().unwrap_or_default().starts_with("/api/get_game/"));
    assert!(files[0]["size"].as_u64().unwrap_or_default() > 0);
}

#[test]
fn cyberfoil_shop_sections_response_shape_fixture_matches_current_surface() {
    let body = json!({
        "success": "Welcome to ownfoil-rs",
        "sections": [
            {
                "id": "new",
                "title": "New Games",
                "items": [
                    {
                        "id": "1",
                        "name": "Demo",
                        "title_id": "0100000000000000",
                        "app_id": "0100000000000000",
                        "app_type": "GAME",
                        "version": 0,
                        "url": "/api/get_game/1#Demo.nsp",
                        "size": 4096
                    }
                ]
            }
        ]
    });

    let sections = body["sections"].as_array().expect("sections array");
    let first_section = sections[0].as_object().expect("section object");
    assert!(first_section.contains_key("id"));
    assert!(first_section.contains_key("title"));
    assert!(first_section.contains_key("items"));

    let item = first_section["items"][0].as_object().expect("section item object");
    for key in ["id", "name", "title_id", "app_id", "app_type", "url", "size"] {
        assert!(item.contains_key(key), "missing shop section item key: {key}");
    }
}

#[test]
fn current_api_titles_alias_shape_is_catalog_not_upstream_games_shape() {
    let body = json!({
        "total": 1,
        "success": "ok",
        "files": [
            {
                "id": "1",
                "url": "/api/get_game/1#Demo.nsp",
                "size": 4096,
                "name": "Demo.nsp"
            }
        ],
        "entries": [
            {
                "id": "1",
                "name": "Demo.nsp",
                "title_id": "0100000000000000",
                "version": 0,
                "kind": "base",
                "type": "base",
                "size": 4096,
                "url": "/api/get_game/1#Demo.nsp"
            }
        ]
    });

    assert_catalog_alias_shape(&body);
    assert!(
        body.get("games").is_none(),
        "upstream /api/titles parity expects {{ total, games }}; current route is a catalog alias"
    );
}

#[test]
fn save_sync_stub_response_shape_is_empty_success_list() {
    let body = json!({
        "success": true,
        "saves": []
    });

    assert_eq!(body["success"], Value::Bool(true));
    assert_eq!(body["saves"].as_array().map(Vec::len), Some(0));
}

#[test]
#[ignore = "upstream browser pages are not route-compatible yet: /settings, /setup, /profile, /login, /logout"]
#[allow(clippy::const_is_empty)]
fn missing_browser_admin_settings_routes_settings_setup_profile_login_logout() {
    let desired_routes = ["/settings", "/setup", "/profile", "/login", "/logout"];
    assert!(desired_routes.is_empty(), "implement browser page parity routes: {desired_routes:?}");
}

#[test]
#[ignore = "upstream settings API is not implemented yet"]
#[allow(clippy::const_is_empty)]
fn missing_upstream_settings_api_routes_settings_titles_shop_library_scheduler() {
    let desired_routes = [
        "GET /api/settings",
        "POST /api/settings/titles",
        "POST /api/settings/shop",
        "GET /api/settings/library/paths",
        "POST /api/settings/library/paths",
        "DELETE /api/settings/library/paths",
        "POST /api/settings/library/management",
        "POST /api/settings/scheduler",
    ];
    assert!(desired_routes.is_empty(), "implement settings API parity routes: {desired_routes:?}");
}

#[test]
#[ignore = "upstream users, signup, upload, and manual scan APIs are not implemented yet"]
#[allow(clippy::const_is_empty)]
fn missing_upstream_users_upload_and_scan_routes_users_user_signup_upload_library_scan() {
    let desired_routes = [
        "GET /api/users",
        "DELETE /api/user",
        "POST /api/user/signup",
        "POST /api/upload",
        "POST /api/library/scan",
    ];
    assert!(
        desired_routes.is_empty(),
        "implement users/upload/scan API parity routes: {desired_routes:?}"
    );
}

#[test]
#[ignore = "current /api/titles is a catalog alias; upstream expects { total, games } with title/app fields"]
fn missing_api_titles_upstream_games_shape_currently_catalog_alias() {
    let upstream_shape = json!({
        "total": 1,
        "games": [
            {
                "id": 1,
                "title_id": "0100000000000000",
                "name": "Demo",
                "icon_url": "/api/title/0100000000000000/icon",
                "banner_url": "/api/title/0100000000000000/banner",
                "apps": []
            }
        ]
    });

    assert!(
        upstream_shape.get("files").is_some(),
        "replace catalog alias with upstream /api/titles games shape: {upstream_shape}"
    );
}

#[test]
#[ignore = "persistent Ownfoil state is pending: libraries, files, titles, apps, users, download counts, identification"]
#[allow(clippy::const_is_empty)]
fn missing_persistent_db_route_state_download_counts_users_identification_status() {
    let required_state_tables = [
        "libraries",
        "files",
        "titles",
        "apps",
        "users",
        "download_counts",
        "identification_status",
    ];
    assert!(
        required_state_tables.is_empty(),
        "add persistent route-backed state before enabling this parity test: {required_state_tables:?}"
    );
}

fn assert_catalog_alias_shape(body: &Value) {
    assert!(body["total"].as_u64().is_some());
    assert!(body["success"].is_string());
    assert!(body["files"].as_array().is_some());
    assert!(body["entries"].as_array().is_some());
}
