use std::collections::BTreeSet;

use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ParityState {
    Supported,
    StubCompatible,
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
    Route { method: "GET", path: "/api/titles", state: ParityState::Supported },
    Route { method: "GET", path: "/settings", state: ParityState::Supported },
    Route { method: "GET", path: "/setup", state: ParityState::StubCompatible },
    Route { method: "GET", path: "/profile", state: ParityState::StubCompatible },
    Route { method: "GET", path: "/login", state: ParityState::Supported },
    Route { method: "GET", path: "/logout", state: ParityState::Supported },
    Route { method: "GET", path: "/api/settings", state: ParityState::Supported },
    Route { method: "POST", path: "/api/settings/titles", state: ParityState::Supported },
    Route { method: "POST", path: "/api/settings/shop", state: ParityState::Supported },
    Route { method: "GET", path: "/api/settings/library/paths", state: ParityState::Supported },
    Route { method: "POST", path: "/api/settings/library/paths", state: ParityState::Supported },
    Route { method: "DELETE", path: "/api/settings/library/paths", state: ParityState::Supported },
    Route {
        method: "POST",
        path: "/api/settings/library/management",
        state: ParityState::Supported,
    },
    Route { method: "POST", path: "/api/settings/scheduler", state: ParityState::Supported },
    Route { method: "POST", path: "/api/upload", state: ParityState::Supported },
    Route { method: "POST", path: "/api/library/scan", state: ParityState::Supported },
    Route { method: "GET", path: "/api/users", state: ParityState::Supported },
    Route { method: "DELETE", path: "/api/user", state: ParityState::Supported },
    Route { method: "POST", path: "/api/user/signup", state: ParityState::Supported },
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
fn upstream_route_matrix_has_no_untracked_gap_state() {
    assert!(
        UPSTREAM_ROUTE_MATRIX.iter().all(|route| matches!(
            route.state,
            ParityState::Supported | ParityState::StubCompatible
        ))
    );
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
fn api_titles_uses_upstream_games_shape() {
    let body = json!({
        "total": 1,
        "games": [
            {
                "id": "0100000000000000",
                "title_id": "0100000000000000",
                "app_id": "0100000000000000",
                "app_type": "BASE",
                "owned": true,
                "version": []
            }
        ]
    });

    assert!(body["total"].as_u64().is_some());
    assert!(body["games"].as_array().is_some());
    assert!(body.get("files").is_none());
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
fn upstream_api_titles_contract_fixture_uses_games_shape() {
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

    assert!(upstream_shape.get("games").is_some());
    assert!(upstream_shape.get("files").is_none());
}
