#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::module_inception)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use serde_json::Value;
    use tempfile::tempdir;
    use tokio::fs;
    use tokio::sync::RwLock;

    use crate::auth::{AuthSettings, AuthUser};
    use crate::catalog::{Catalog, ContentFile, ContentKind};
    use crate::config::TitleDbConfig;
    use crate::shop::ShopConfig;
    use crate::titledb::{TitleDb, TitleInfo};

    use crate::http::{AppState, router, state::SessionStore};

    fn test_app_state(
        catalog: Catalog,
        library_root: PathBuf,
        auth: AuthSettings,
        sessions: SessionStore,
    ) -> AppState {
        test_app_state_with_options(
            catalog,
            library_root,
            auth,
            sessions,
            false,
            ShopConfig::default(),
        )
    }

    fn test_app_state_with_cookie_mode(
        catalog: Catalog,
        library_root: PathBuf,
        auth: AuthSettings,
        sessions: SessionStore,
        insecure_admin_cookie: bool,
    ) -> AppState {
        test_app_state_with_options(
            catalog,
            library_root,
            auth,
            sessions,
            insecure_admin_cookie,
            ShopConfig::default(),
        )
    }

    fn test_app_state_with_options(
        catalog: Catalog,
        library_root: PathBuf,
        auth: AuthSettings,
        sessions: SessionStore,
        insecure_admin_cookie: bool,
        shop: ShopConfig,
    ) -> AppState {
        let data_dir = std::env::temp_dir().join(format!("ownfoil-test-{}", uuid::Uuid::new_v4()));
        let (progress_tx, _) = tokio::sync::broadcast::channel(1);
        let titledb = TitleDb::with_progress(
            TitleDbConfig { enabled: false, ..Default::default() },
            data_dir.clone(),
            Some(progress_tx.clone()),
        );
        AppState {
            catalog: Arc::new(RwLock::new(catalog)),
            library_root,
            storage: None,
            scan_lock: Arc::new(tokio::sync::Mutex::new(())),
            settings: Arc::new(RwLock::new(crate::settings::Settings::default())),
            settings_path: data_dir.join("settings.yaml"),
            keys_path: data_dir.join("keys.txt"),
            auth: Arc::new(auth),
            shop: Arc::new(RwLock::new(shop)),
            insecure_admin_cookie,
            sessions,
            titledb,
            titles_cache: Arc::new(RwLock::new(None)),
            data_dir,
            titledb_progress_tx: progress_tx,
        }
    }

    fn test_app_state_with_titledb(
        catalog: Catalog,
        library_root: PathBuf,
        auth: AuthSettings,
        sessions: SessionStore,
        titledb: TitleDb,
    ) -> AppState {
        let data_dir = std::env::temp_dir().join(format!("ownfoil-test-{}", uuid::Uuid::new_v4()));
        let (progress_tx, _) = tokio::sync::broadcast::channel(1);
        AppState {
            catalog: Arc::new(RwLock::new(catalog)),
            library_root,
            storage: None,
            scan_lock: Arc::new(tokio::sync::Mutex::new(())),
            settings: Arc::new(RwLock::new(crate::settings::Settings::default())),
            settings_path: data_dir.join("settings.yaml"),
            keys_path: data_dir.join("keys.txt"),
            auth: Arc::new(auth),
            shop: Arc::new(RwLock::new(ShopConfig::default())),
            insecure_admin_cookie: false,
            sessions,
            titledb,
            titles_cache: Arc::new(RwLock::new(None)),
            data_dir,
            titledb_progress_tx: progress_tx,
        }
    }

    #[tokio::test]
    async fn health_returns_ok_with_catalog_count() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            id: 0,
            library_root: PathBuf::new(),
            relative_path: PathBuf::from("a.nsp"),
            name: String::from("a.nsp"),
            size: 1,
            title_id: None,
            version: None,
            kind: ContentKind::Unknown,
            identified_contents: Vec::new(),
        }]);
        let state = test_app_state(
            catalog,
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        let server = TestServer::new(router(state))?;
        let response = server.get("/health").await;
        assert_eq!(response.status_code(), StatusCode::OK);
        let body: Value = response.json();
        assert_eq!(body.get("status"), Some(&Value::String("ok".into())));
        assert_eq!(body.get("catalog_files"), Some(&Value::Number(1_i64.into())));
        Ok(())
    }

    #[tokio::test]
    async fn html_pages_inline_the_shared_theme() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        let server = TestServer::new(router(state))?;

        for path in ["/admin", "/admin/settings", "/setup", "/profile", "/admin/login"] {
            let page = server.get(path).await;
            assert_eq!(page.status_code(), StatusCode::OK, "{path}");
            let html = page.text();
            assert!(html.contains("[data-theme=ownfoil]"), "{path}");
            assert!(html.contains("ownfoil-theme"), "{path}");
            assert!(html.contains("data-theme-picker"), "{path}");
            assert!(!html.contains("<!--theme-->"), "{path}");
            assert!(!html.contains("/assets/ownfoil"), "{path}");
        }

        let admin = server.get("/admin").await.text();
        assert!(admin.contains("name=\"description\""));
        assert!(admin.contains("loading=\"lazy\""));
        assert!(admin.contains("/api/graphql"));
        assert!(!admin.contains("<h3"));

        let compressed = server.get("/admin").add_header("Accept-Encoding", "gzip").await;
        assert_eq!(compressed.header("content-encoding"), "gzip");

        let robots = server.get("/robots.txt").await;
        assert_eq!(robots.status_code(), StatusCode::OK);
        assert_eq!(robots.text(), "User-agent: *\nDisallow:\n");
        assert_eq!(server.get("/favicon.ico").await.status_code(), StatusCode::NO_CONTENT);
        Ok(())
    }

    #[tokio::test]
    async fn download_supports_range() -> Result<()> {
        let dir = tempdir()?;
        let file_path = dir.path().join("demo.nsp");
        fs::write(&file_path, b"0123456789").await?;

        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            dir.path().to_path_buf(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;

        let response = server.get("/api/download/demo.nsp").add_header("Range", "bytes=1-3").await;

        assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.header("accept-ranges"), "bytes");
        assert_eq!(response.text(), "123");
        Ok(())
    }

    #[tokio::test]
    async fn get_game_by_id_supports_range() -> Result<()> {
        let dir = tempdir()?;
        let file_path = dir.path().join("demo.nsp");
        fs::write(&file_path, b"0123456789").await?;

        let catalog = Catalog::from_files(vec![ContentFile {
            id: 0,
            library_root: PathBuf::new(),
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
            identified_contents: Vec::new(),
        }]);

        let state = test_app_state(
            catalog,
            dir.path().to_path_buf(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/get_game/1").add_header("Range", "bytes=1-3").await;

        assert_eq!(response.status_code(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.header("accept-ranges"), "bytes");
        assert_eq!(response.text(), "123");
        Ok(())
    }

    #[tokio::test]
    async fn catalog_requires_basic_auth_when_enabled() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(vec![AuthUser {
                username: String::from("admin"),
                password: String::from("secret"),
            }]),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;

        let unauthorized = server.get("/api/catalog").await;
        assert_eq!(unauthorized.status_code(), StatusCode::UNAUTHORIZED);
        assert_eq!(unauthorized.header("www-authenticate"), "Basic realm=\"ownfoil-rs\"");

        let authorized =
            server.get("/api/catalog").add_header("Authorization", "Basic YWRtaW46d3Jvbmc=").await;
        assert_eq!(authorized.status_code(), StatusCode::UNAUTHORIZED);

        let authorized =
            server.get("/api/catalog").add_header("Authorization", "Basic YWRtaW46c2VjcmV0").await;
        assert_eq!(authorized.status_code(), StatusCode::OK);

        let authorized =
            server.get("/api/catalog").add_header("Authorization", "YWRtaW46c2VjcmV0").await;
        assert_eq!(authorized.status_code(), StatusCode::UNAUTHORIZED);
        Ok(())
    }

    #[tokio::test]
    async fn auth_settings_compat_routes_require_admin_auth() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(vec![AuthUser {
                username: String::from("admin"),
                password: String::from("secret"),
            }]),
            SessionStore::new(24),
        );
        let server = TestServer::new(router(state))?;

        let unauthorized = server.get("/api/settings").await;
        assert_eq!(unauthorized.status_code(), StatusCode::UNAUTHORIZED);

        let settings =
            server.get("/api/settings").add_header("Authorization", "Basic YWRtaW46c2VjcmV0").await;
        assert_eq!(settings.status_code(), StatusCode::OK);
        let body: Value = settings.json();
        assert_eq!(body.pointer("/library/paths/0"), Some(&Value::String("/games".into())));
        assert_eq!(body.pointer("/titles/region"), Some(&Value::String("US".into())));

        let users =
            server.get("/api/users").add_header("Authorization", "Basic YWRtaW46c2VjcmV0").await;
        assert_eq!(users.status_code(), StatusCode::OK);
        let body: Value = users.json();
        assert_eq!(body.pointer("/0/user"), Some(&Value::String("admin".into())));

        Ok(())
    }

    #[tokio::test]
    async fn mutable_settings_routes_persist_compatible_payloads() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(vec![AuthUser {
                username: String::from("admin"),
                password: String::from("secret"),
            }]),
            SessionStore::new(24),
        );
        let server = TestServer::new(router(state))?;

        let cases = [
            ("/api/settings/titles", serde_json::json!({"region": "EU", "language": "fr"})),
            ("/api/settings/shop", serde_json::to_value(crate::settings::ShopSettings::default())?),
            (
                "/api/settings/library/management",
                serde_json::to_value(crate::settings::LibraryManagementSettings::default())?,
            ),
            ("/api/settings/scheduler", serde_json::json!({"scan_interval": "30m"})),
        ];
        for (path, payload) in cases {
            let response = server
                .post(path)
                .add_header("Authorization", "Basic YWRtaW46c2VjcmV0")
                .json(&payload)
                .await;
            assert_eq!(response.status_code(), StatusCode::OK, "{path}: {}", response.text());
            let body: Value = response.json();
            assert_eq!(body.get("success"), Some(&Value::Bool(true)), "{path}");
        }

        Ok(())
    }

    #[tokio::test]
    async fn settings_routes_allow_first_admin_setup_when_auth_disabled() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let settings = server.get("/api/settings").await;
        assert_eq!(settings.status_code(), StatusCode::OK);
        let login = server.get("/admin/login").await;
        assert_eq!(login.status_code(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn admin_login_sets_secure_cookie_by_default() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(vec![AuthUser {
                username: String::from("admin"),
                password: String::from("secret"),
            }]),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server
            .post("/admin/login")
            .content_type("application/x-www-form-urlencoded")
            .bytes("username=admin&password=secret".into())
            .await;
        assert_eq!(response.status_code(), StatusCode::SEE_OTHER);
        let set_cookie = response.header("set-cookie");
        let set_cookie = set_cookie.to_str().unwrap_or_default();
        assert!(set_cookie.contains("Secure"));
        Ok(())
    }

    #[tokio::test]
    async fn admin_login_allows_insecure_cookie_when_configured() -> Result<()> {
        let state = test_app_state_with_cookie_mode(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(vec![AuthUser {
                username: String::from("admin"),
                password: String::from("secret"),
            }]),
            SessionStore::new(24),
            true,
        );

        let server = TestServer::new(router(state))?;
        let response = server
            .post("/admin/login")
            .content_type("application/x-www-form-urlencoded")
            .bytes("username=admin&password=secret".into())
            .await;
        assert_eq!(response.status_code(), StatusCode::SEE_OTHER);
        let set_cookie = response.header("set-cookie");
        let set_cookie = set_cookie.to_str().unwrap_or_default();
        assert!(!set_cookie.contains("Secure"));
        Ok(())
    }

    #[tokio::test]
    async fn shop_response_contains_files_list() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            id: 0,
            library_root: PathBuf::new(),
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
            identified_contents: Vec::new(),
        }]);

        let state = test_app_state(
            catalog,
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/shop").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let files = body.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(files.len(), 1);
        let first = files[0].as_object().cloned().unwrap_or_default();
        assert_eq!(
            first.get("url"),
            Some(&Value::String(String::from("/api/get_game/1#demo.nsp")))
        );
        assert_eq!(first.get("size"), Some(&Value::Number(10_u64.into())));
        assert_eq!(body.get("success"), Some(&Value::String(String::from("ok"))));
        Ok(())
    }

    #[tokio::test]
    async fn shop_root_returns_tinfoil_payload_when_encryption_enabled() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            id: 0,
            library_root: PathBuf::new(),
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
            identified_contents: Vec::new(),
        }]);

        let state = test_app_state_with_options(
            catalog,
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
            false,
            ShopConfig { encrypt: true, ..Default::default() },
        );

        let server = TestServer::new(router(state))?;
        let response = server
            .get("/")
            .add_header("Theme", "dark")
            .add_header("Uid", "test")
            .add_header("Version", "18.0")
            .add_header("Revision", "1")
            .add_header("Language", "en")
            .add_header("Hauth", "")
            .add_header("Uauth", "")
            .await;

        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(response.header("content-type"), "application/octet-stream");
        let body = response.as_bytes();
        assert!(body.starts_with(b"TINFOIL"));
        Ok(())
    }

    #[tokio::test]
    async fn browser_root_redirects_to_admin_when_html_is_preferred() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(vec![AuthUser {
                username: String::from("admin"),
                password: String::from("secret"),
            }]),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server
            .get("/")
            .add_header("Accept", "text/html")
            .add_header("User-Agent", "Mozilla/5.0")
            .await;

        assert_eq!(response.status_code(), StatusCode::SEE_OTHER);
        assert_eq!(response.header("location"), "/login");
        Ok(())
    }

    #[tokio::test]
    async fn root_response_contains_files_list() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            id: 0,
            library_root: PathBuf::new(),
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
            identified_contents: Vec::new(),
        }]);

        let state = test_app_state(
            catalog,
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let files = body.get("files").and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(files.len(), 1);
        let first = files[0].as_object().cloned().unwrap_or_default();
        assert_eq!(
            first.get("url"),
            Some(&Value::String(String::from("/api/get_game/1#demo.nsp")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn shop_sections_returns_section_items() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            id: 0,
            library_root: PathBuf::new(),
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
            identified_contents: Vec::new(),
        }]);

        let state = test_app_state(
            catalog,
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/sections").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let sections = body.get("sections").and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(sections.len(), 5);

        let first_section =
            sections.first().and_then(Value::as_object).cloned().unwrap_or_default();
        assert_eq!(first_section.get("id"), Some(&Value::String(String::from("new"))));
        let items =
            first_section.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
        assert_eq!(items.len(), 1);

        let first_item = items[0].as_object().cloned().unwrap_or_default();
        assert_eq!(
            first_item.get("url"),
            Some(&Value::String(String::from("/api/get_game/1#demo.nsp")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn shop_sections_returns_tinfoil_payload_when_encryption_enabled() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            id: 0,
            library_root: PathBuf::new(),
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
            identified_contents: Vec::new(),
        }]);

        let state = test_app_state_with_options(
            catalog,
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
            false,
            ShopConfig { encrypt: true, ..Default::default() },
        );

        let server = TestServer::new(router(state))?;
        let response = server
            .get("/api/shop/sections")
            .add_header("Theme", "dark")
            .add_header("Uid", "test")
            .add_header("Version", "18.0")
            .add_header("Revision", "1")
            .add_header("Language", "en")
            .add_header("Hauth", "")
            .add_header("Uauth", "")
            .await;

        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(response.header("content-type"), "application/octet-stream");
        let body = response.as_bytes();
        assert!(body.starts_with(b"TINFOIL"));
        Ok(())
    }

    #[tokio::test]
    async fn shop_sections_new_falls_back_to_all_when_no_base_items() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            id: 0,
            library_root: PathBuf::new(),
            relative_path: PathBuf::from("update.nsp"),
            name: String::from("update.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000800")),
            version: Some(65536),
            kind: ContentKind::Update,
            identified_contents: Vec::new(),
        }]);

        let state = test_app_state(
            catalog,
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/sections").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let sections = body.get("sections").and_then(Value::as_array).cloned().unwrap_or_default();
        let new_items = sections
            .iter()
            .find(|section| section.get("id") == Some(&Value::String(String::from("new"))))
            .and_then(|section| section.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(new_items.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn update_section_item_uses_base_title_id_and_update_app_id() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            id: 0,
            library_root: PathBuf::new(),
            relative_path: PathBuf::from("update.nsp"),
            name: String::from("update.nsp"),
            size: 10,
            title_id: Some(String::from("0100ABCD12340800")),
            version: Some(65536),
            kind: ContentKind::Update,
            identified_contents: Vec::new(),
        }]);

        let state = test_app_state(
            catalog,
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/sections").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let updates = body
            .get("sections")
            .and_then(Value::as_array)
            .and_then(|sections| {
                sections.iter().find(|section| {
                    section.get("id") == Some(&Value::String(String::from("updates")))
                })
            })
            .and_then(|section| section.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        assert_eq!(updates.len(), 1);
        let item = updates[0].as_object().cloned().unwrap_or_default();
        assert_eq!(item.get("title_id"), Some(&Value::String(String::from("0100ABCD12340000"))));
        assert_eq!(item.get("app_id"), Some(&Value::String(String::from("0100ABCD12340800"))));
        assert_eq!(item.get("app_type"), Some(&Value::String(String::from("UPDATE"))));
        Ok(())
    }

    #[tokio::test]
    async fn dlc_section_item_uses_base_title_id_and_dlc_app_id() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            id: 0,
            library_root: PathBuf::new(),
            relative_path: PathBuf::from("dlc.nsp"),
            name: String::from("dlc.nsp"),
            size: 10,
            title_id: Some(String::from("0100ABCD12341001")),
            version: Some(0),
            kind: ContentKind::Dlc,
            identified_contents: Vec::new(),
        }]);

        let state = test_app_state(
            catalog,
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/sections").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let dlc = body
            .get("sections")
            .and_then(Value::as_array)
            .and_then(|sections| {
                sections
                    .iter()
                    .find(|section| section.get("id") == Some(&Value::String(String::from("dlc"))))
            })
            .and_then(|section| section.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        assert_eq!(dlc.len(), 1);
        let item = dlc[0].as_object().cloned().unwrap_or_default();
        assert_eq!(item.get("title_id"), Some(&Value::String(String::from("0100ABCD12340000"))));
        assert_eq!(item.get("app_id"), Some(&Value::String(String::from("0100ABCD12341001"))));
        assert_eq!(item.get("app_type"), Some(&Value::String(String::from("DLC"))));
        Ok(())
    }

    #[tokio::test]
    async fn updates_section_keeps_only_latest_version_per_base_title() -> Result<()> {
        let catalog = Catalog::from_files(vec![
            ContentFile {
                id: 0,
                library_root: PathBuf::new(),
                relative_path: PathBuf::from("update-old.nsp"),
                name: String::from("update-old.nsp"),
                size: 10,
                title_id: Some(String::from("0100ABCD12340800")),
                version: Some(65536),
                kind: ContentKind::Update,
                identified_contents: Vec::new(),
            },
            ContentFile {
                id: 0,
                library_root: PathBuf::new(),
                relative_path: PathBuf::from("update-new.nsp"),
                name: String::from("update-new.nsp"),
                size: 10,
                title_id: Some(String::from("0100ABCD12340800")),
                version: Some(131_072),
                kind: ContentKind::Update,
                identified_contents: Vec::new(),
            },
        ]);

        let state = test_app_state(
            catalog,
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/sections?limit=50").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        let updates = body
            .get("sections")
            .and_then(Value::as_array)
            .and_then(|sections| {
                sections.iter().find(|section| {
                    section.get("id") == Some(&Value::String(String::from("updates")))
                })
            })
            .and_then(|section| section.get("items"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        assert_eq!(updates.len(), 1);
        let item = updates[0].as_object().cloned().unwrap_or_default();
        assert_eq!(item.get("app_version"), Some(&Value::String(String::from("131072"))));
        Ok(())
    }

    #[tokio::test]
    async fn shop_icon_route_returns_image() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/icon/0100000000000000.png").await;

        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(response.header("content-type"), "image/svg+xml");
        assert_eq!(response.header("cache-control"), "public, max-age=604800, immutable");
        Ok(())
    }

    #[tokio::test]
    async fn shop_icon_route_redirects_to_titledb_icon_url() -> Result<()> {
        let titledb = TitleDb::from_entries(vec![(
            String::from("0100000000000000"),
            TitleInfo {
                record: serde_json::Map::default(),
                icon_url: Some(String::from("https://example.test/icon.png")),
                banner_url: None,
                name: Some(String::from("Example Game")),
            },
        )]);
        let state = test_app_state_with_titledb(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
            titledb,
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/icon/0100000000000000.png").await;

        assert_eq!(response.status_code(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.header("location"), "https://example.test/icon.png");
        Ok(())
    }

    #[tokio::test]
    async fn shop_banner_route_redirects_to_titledb_banner_url() -> Result<()> {
        let titledb = TitleDb::from_entries(vec![(
            String::from("0100000000000000"),
            TitleInfo {
                record: serde_json::Map::default(),
                icon_url: None,
                banner_url: Some(String::from("https://example.test/banner.jpg")),
                name: Some(String::from("Example Game")),
            },
        )]);
        let state = test_app_state_with_titledb(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
            titledb,
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/shop/banner/0100000000000000.png").await;

        assert_eq!(response.status_code(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.header("location"), "https://example.test/banner.jpg");
        Ok(())
    }

    #[tokio::test]
    async fn saves_list_endpoint_returns_empty_success_payload() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server.get("/api/saves/list").await;
        assert_eq!(response.status_code(), StatusCode::OK);

        let body: Value = response.json();
        assert_eq!(body.get("success"), Some(&Value::Bool(true)));
        assert_eq!(body.get("saves").and_then(Value::as_array).map(Vec::len), Some(0));
        Ok(())
    }
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // Seed the upstream fixture and compare all role-specific responses.
    async fn graphql_matches_pinned_upstream_response_matrix() -> Result<()> {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/graphql_parity.json"))?;
        let directory = tempdir()?;
        let storage = crate::storage::Storage::open(directory.path().join("parity.db")).await?;
        let seed = fixture.clone();
        storage.with_connection(move |conn| {
            conn.execute("INSERT INTO libraries(id,path) VALUES(1,'/parity/games')", [])?;
            conn.execute("INSERT INTO titles(id,title_id,have_base,up_to_date,complete) VALUES(1,?1,1,0,1)", [seed["title"].as_str()])?;
            for (index, app) in seed["apps"].as_array().unwrap().iter().enumerate() {
                let id = i64::try_from(index + 1).unwrap();
                conn.execute("INSERT INTO apps(id,title_id,app_id,app_type,app_version,owned) VALUES(?1,1,?2,?3,?4,?5)", rusqlite::params![id,app[0].as_str(),app[1].as_str(),app[2].as_str(),app[3].as_bool()])?;
                if let Some(filename) = app[4].as_str() {
                    conn.execute("INSERT INTO files(library_id,path,folder,name,ext,size,title_id,identification_status) VALUES(1,?1,'/parity/games',?1,'nsp',?2,?3,'identified')", rusqlite::params![filename, app[5].as_i64(),seed["title"].as_str()])?;
                    conn.execute("INSERT INTO app_files(app_id,file_id) VALUES(?1,?2)", [id,conn.last_insert_rowid()])?;
                }
            }
            conn.execute("INSERT INTO tasks(id,task_name,status,completion_pct,input_json) VALUES(1,'scan_libraries','completed',100,'{\"path\": \"/games\"}')", [])?;
            conn.execute("INSERT INTO tasks(id,parent_id,task_name,status,completion_pct) VALUES(2,1,'scan_library','running',40)", [])?;
            Ok(())
        }).await?;
        let mut files = Vec::new();
        for app in fixture["apps"].as_array().unwrap() {
            if let Some(filename) = app[4].as_str() {
                let kind = match app[1].as_str().unwrap() {
                    "UPDATE" => ContentKind::Update,
                    "DLC" => ContentKind::Dlc,
                    _ => ContentKind::Base,
                };
                files.push(ContentFile {
                    id: files.len() + 1,
                    library_root: "/parity/games".into(),
                    relative_path: filename.into(),
                    name: filename.into(),
                    size: app[5].as_u64().unwrap(),
                    title_id: Some(app[0].as_str().unwrap().into()),
                    version: Some(app[2].as_str().unwrap().parse()?),
                    kind,
                    identified_contents: Vec::new(),
                });
            }
        }
        let auth = AuthSettings::from_users(Vec::new());
        let sessions = SessionStore::new(24);
        for (name, admin_access, shop_access) in
            [("full", true, true), ("shop", false, true), ("admin", true, false)]
        {
            auth.upsert_hashed_user(
                name.into(),
                "unused-session-test".into(),
                crate::auth::AuthRoles { admin_access, shop_access, backup_access: false },
            );
        }
        let mut state = test_app_state(
            Catalog::from_files(files),
            "/parity/games".into(),
            auth,
            sessions.clone(),
        );
        state.storage = Some(storage);
        state.titledb = TitleDb::from_entries(fixture["metadata"].as_object().unwrap().iter().map(
            |(id, record)| {
                (
                    id.clone(),
                    TitleInfo {
                        name: record["name"].as_str().map(str::to_owned),
                        record: record.as_object().unwrap().clone(),
                        ..Default::default()
                    },
                )
            },
        ));
        // Exercise the real endpoint without the unrelated 50-request burst limiter.
        let server = TestServer::new(
            axum::Router::new()
                .route("/api/graphql", axum::routing::post(crate::http::graphql::post))
                .with_state(state),
        )?;
        let mut failures = Vec::new();
        for (index, case) in fixture["cases"].as_array().unwrap().iter().enumerate() {
            let name = if case["admin"] == false {
                "shop"
            } else if case["shop"] == false {
                "admin"
            } else {
                "full"
            };
            let response = server
                .post("/api/graphql")
                .add_header("Cookie", format!("ownfoil_session={}", sessions.create(name.into())))
                .json(&serde_json::json!({"query":case["query"]}))
                .await;
            response.assert_status_ok();
            let actual = response.json::<Value>();
            if actual.get("errors").is_some() || actual["data"] != case["data"] {
                failures.push(format!(
                    "case {index} ({name}): {}\nexpected: {}\nactual: {}",
                    case["query"], case["data"], actual
                ));
            }
        }
        assert!(failures.is_empty(), "{} mismatches:\n{}", failures.len(), failures.join("\n"));
        Ok(())
    }

    #[tokio::test]
    async fn graphql_contract_queries_validate_and_paginate() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        let server = TestServer::new(router(state))?;
        let response=server.post("/api/graphql").json(&serde_json::json!({"query":"{ titles(page:0,pageSize:900) { total items { titleId name } } stats { totalFiles totalSize ownedApps } __type(name: \"Mutation\") { fields { name } } }"})).await;
        response.assert_status_ok();
        let value = response.json::<Value>();
        assert!(value.get("errors").is_none(), "{value}");
        assert_eq!(value["data"]["titles"]["total"], 0);
        let invalid = server
            .post("/api/graphql")
            .json(&serde_json::json!({"query":"{ stats { nonexistent } }"}))
            .await
            .json::<Value>();
        assert!(invalid["errors"].is_array());
        let get = server
            .get("/api/graphql")
            .add_query_param("query", "mutation { purgeFailedTasks }")
            .await;
        assert_eq!(get.status_code(), StatusCode::METHOD_NOT_ALLOWED);
        Ok(())
    }

    #[tokio::test]
    async fn graphql_independent_roles_and_case_insensitive_title_lookup() -> Result<()> {
        let auth = AuthSettings::from_users(Vec::new());
        let sessions = SessionStore::new(24);
        for (name, admin_access, shop_access) in [("admin", true, false), ("shop", false, true)] {
            auth.upsert_hashed_user(
                name.into(),
                "unused-in-session-test".into(),
                crate::auth::AuthRoles { admin_access, shop_access, backup_access: false },
            );
        }
        let state = test_app_state(
            Catalog::from_files(vec![ContentFile {
                id: 1,
                library_root: std::env::temp_dir(),
                relative_path: "demo.nsp".into(),
                name: "demo.nsp".into(),
                size: 10,
                title_id: Some("01000000000AB000".into()),
                version: Some(0),
                kind: ContentKind::Base,
                identified_contents: Vec::new(),
            }]),
            std::env::temp_dir(),
            auth,
            sessions.clone(),
        );
        let server = TestServer::new(router(state))?;
        let query = serde_json::json!({"query": "{ titles(owned: true) { total } apps { total items { id } } title(titleId: \"01000000000ab000\") { titleId } files { total } stats { totalTitles totalFiles appsByType { key } } }"});
        for (name, shop) in [("admin", false), ("shop", true)] {
            let cookie = format!("ownfoil_session={}", sessions.create(name.into()));
            let response =
                server.post("/api/graphql").add_header("Cookie", &cookie).json(&query).await;
            response.assert_status_ok();
            let body = response.json::<Value>();
            assert!(body.get("errors").is_none(), "{body}");
            let data = &body["data"];
            assert_eq!(data["titles"]["total"], usize::from(shop));
            assert_eq!(data["apps"]["total"], usize::from(shop));
            assert_eq!(data["files"]["total"], usize::from(!shop));
            assert_eq!(data["stats"]["totalTitles"], 0);
            assert_eq!(data["stats"]["totalFiles"], 0);
            if shop {
                assert_eq!(data["title"]["titleId"], "01000000000AB000");
                assert!(data["stats"]["appsByType"].is_array());
            } else {
                assert!(data["title"].is_null());
                assert!(data["stats"]["appsByType"].is_null());
            }
            let tasks = server
                .post("/api/graphql")
                .add_header("Cookie", &cookie)
                .json(
                    &serde_json::json!({"query":"{ tasks(taskName: \"not_registered\") { id } }"}),
                )
                .await
                .json::<Value>();
            if shop {
                assert!(tasks.get("errors").is_none(), "{tasks}");
                assert_eq!(tasks["data"]["tasks"], serde_json::json!([]));
            } else {
                assert!(tasks["errors"].is_array());
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn graphql_requires_identity_on_public_shops_and_respects_roles_and_etags() -> Result<()>
    {
        let auth = AuthSettings::from_users(vec![AuthUser {
            username: "admin".into(),
            password: "secret".into(),
        }]);
        auth.upsert_hashed_user(
            "guest".into(),
            "unused-in-session-test".into(),
            crate::auth::AuthRoles { admin_access: false, shop_access: true, backup_access: false },
        );
        let sessions = SessionStore::new(24);
        let token = sessions.create("guest".into());
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            auth,
            sessions.clone(),
        );
        state.settings.write().await.shop.public = true;
        let server = TestServer::new(router(state))?;
        server.get("/api/graphql").await.assert_status_unauthorized();
        let cookie = format!("ownfoil_session={token}");
        let query = serde_json::json!({"query":"{ files { total } tasks { id } workers { id } }"});
        let response = server.post("/api/graphql").add_header("Cookie", &cookie).json(&query).await;
        response.assert_status_ok();
        let data = response.json::<Value>();
        assert!(data.get("errors").is_none(), "{data}");
        assert_eq!(data["data"]["files"]["total"], 0);
        assert_eq!(data["data"]["tasks"], serde_json::json!([]));
        assert_eq!(data["data"]["workers"], serde_json::json!([]));
        let etag = response.header("etag");
        server
            .post("/api/graphql")
            .add_header("Cookie", &cookie)
            .add_header("if-none-match", etag)
            .json(&query)
            .await
            .assert_status(StatusCode::NOT_MODIFIED);
        let mutation = server
            .post("/api/graphql")
            .add_header("Cookie", &cookie)
            .json(&serde_json::json!({"query":"mutation { purgeFailedTasks }"}))
            .await;
        assert!(mutation.json::<Value>()["errors"].is_array());
        assert_eq!(mutation.header("cache-control"), "no-store");
        sessions.remove(&token);
        server
            .post("/api/graphql")
            .add_header("Cookie", &cookie)
            .json(&query)
            .await
            .assert_status_unauthorized();
        Ok(())
    }

    #[tokio::test]
    async fn current_settings_preserve_partial_updates_and_reject_invalid_limits() -> Result<()> {
        let dir = tempdir()?;
        let mut state = test_app_state(
            Catalog::from_files(Vec::new()),
            dir.path().into(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        state.settings_path = dir.path().join("settings.yaml");
        let server = TestServer::new(router(state))?;
        let response = server
            .post("/api/settings/library/watcher")
            .json(&serde_json::json!({"enabled":false}))
            .await;
        assert_eq!(response.json::<Value>()["success"], true);
        let invalid = server
            .post("/api/settings/worker")
            .json(&serde_json::json!({"count":0}))
            .await
            .json::<Value>();
        assert_eq!(invalid["success"], false);
        let response = server
            .post("/api/settings/scheduler")
            .json(&serde_json::json!({"titledb_update_interval":"30m"}))
            .await
            .json::<Value>();
        assert_eq!(response["success"], true);
        let settings = server.get("/api/settings").await.json::<Value>();
        assert_eq!(settings["library"]["watcher"]["enabled"], false);
        assert_eq!(settings["library"]["watcher"]["polling_interval"], 60);
        assert_eq!(settings["worker"]["count"], 2);
        assert_eq!(settings["scheduler"]["titledb_update_interval"], "30m");
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires OWNFOIL_TEST_ARCHIVE and OWNFOIL_TEST_KEYS; only temporary copies are modified"]
    async fn local_archive_conversion_roundtrip() -> Result<()> {
        let source = PathBuf::from(
            std::env::var_os("OWNFOIL_TEST_ARCHIVE").context("Set OWNFOIL_TEST_ARCHIVE")?,
        );
        let keys =
            PathBuf::from(std::env::var_os("OWNFOIL_TEST_KEYS").context("Set OWNFOIL_TEST_KEYS")?);
        let dir = tempdir()?;
        let games = dir.path().join("games");
        std::fs::create_dir(&games)?;
        let input = games.join(source.file_name().context("Archive needs a filename")?);
        std::fs::copy(&source, &input).context("Copy input archive")?;
        let mut state = test_app_state(
            Catalog::from_files(Vec::new()),
            games.clone(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        state.keys_path = keys;
        state.data_dir = dir.path().join("data");
        std::fs::create_dir(&state.data_dir)?;
        let storage = crate::storage::Storage::open(state.data_dir.join("test.db")).await?;
        state.storage = Some(storage.clone());
        {
            let mut settings = state.settings.write().await;
            settings.library.paths = vec![games.clone()];
            settings.library.management.organizer.enabled = false;
            settings.library.management.compression.level = 1;
            settings.library.management.compression.block_size_exponent = 20;
            settings.library.management.compression.mode =
                std::env::var("OWNFOIL_TEST_MODE").unwrap_or_else(|_| "solid".into());
        }
        let management = state.settings.read().await.library.management.clone();
        let files = crate::scan_all_libraries(
            std::slice::from_ref(&games),
            &storage,
            &management,
            &state.keys_path,
        )
        .await?;
        anyhow::ensure!(files.len() == 1, "Expected one archive in isolated test library");
        let id = files[0].id;
        *state.catalog.write().await = Catalog::from_files(files);
        let input = serde_json::json!({"file_id":id});
        let task = crate::tasks::enqueue(&storage, "verify_file", input.clone()).await?;
        let task_id = task["id"].as_str().unwrap().parse::<i64>()?;
        let before = crate::content::run(&state, task_id, "verify_file", &input)
            .await
            .context("Verify test copy")?;
        anyhow::ensure!(
            before["hashValid"] == true,
            "Original archive verification failed: {}",
            before["verificationError"]
        );
        let compressed = matches!(source.extension().and_then(|s| s.to_str()), Some("nsz" | "xcz"));
        let directions = if compressed {
            ["decompress_file", "compress_file"]
        } else {
            ["compress_file", "decompress_file"]
        };
        for direction in directions {
            let task = crate::tasks::enqueue(&storage, direction, input.clone()).await?;
            let task_id = task["id"].as_str().unwrap().parse::<i64>()?;
            crate::content::run(&state, task_id, direction, &input)
                .await
                .with_context(|| format!("Conversion {direction}"))?;
            assert!(storage.get_file(i64::try_from(id)?).await?.is_some());
        }
        let after = crate::content::run(&state, task_id, "verify_file", &input)
            .await
            .context("Verify test copy")?;
        assert_eq!(after["hashValid"], true, "{after}");
        assert_eq!(before["signatureValid"], after["signatureValid"]);
        assert!(source.exists());
        Ok(())
    }

    #[tokio::test]
    async fn conversion_recovery_preserves_ids_and_rejects_changed_payloads() -> Result<()> {
        let dir = tempdir()?;
        let games = dir.path().join("games");
        std::fs::create_dir(&games)?;
        let mut state = test_app_state(
            Catalog::from_files(Vec::new()),
            games.clone(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        state.data_dir = dir.path().join("data");
        std::fs::create_dir(&state.data_dir)?;
        let storage = crate::storage::Storage::open(state.data_dir.join("test.db")).await?;
        state.storage = Some(storage.clone());
        state.settings.write().await.library.paths = vec![games.clone()];
        let mut bytes = b"PFS0".to_vec();
        bytes.extend(1u32.to_le_bytes());
        bytes.extend(9u32.to_le_bytes());
        bytes.extend([0; 4]);
        bytes.extend(0u64.to_le_bytes());
        bytes.extend(3u64.to_le_bytes());
        bytes.extend([0; 8]);
        bytes.extend(b"file.bin\0abc");
        let source = games.join("Demo.nsp");
        let target = games.join("Demo.nsz");
        std::fs::write(&source, &bytes)?;
        let files = crate::scan_all_libraries(
            std::slice::from_ref(&games),
            &storage,
            &state.settings.read().await.library.management,
            &state.keys_path,
        )
        .await?;
        let id = files[0].id;
        *state.catalog.write().await = Catalog::from_files(files);
        let journal = state.data_dir.join(format!("conversion-{id}.json"));
        let canonical_root = std::fs::canonicalize(&games)?;
        let data = serde_json::json!({"source":std::fs::canonicalize(&source)?,"target":canonical_root.join("Demo.nsz"),"root":canonical_root,"library_path":games,"file_id":id});
        std::fs::write(&journal, data.to_string())?;
        // Crash before target publication keeps the source and removes the stale journal.
        crate::content::recover(&state).await?;
        assert!(source.exists());
        assert!(!journal.exists());
        std::fs::write(&journal, data.to_string())?;
        let mut damaged = bytes.clone();
        *damaged.last_mut().unwrap() = b'd';
        std::fs::write(&target, damaged)?;
        assert!(crate::content::recover(&state).await.is_err());
        assert!(source.exists() && journal.exists());
        // After publication, valid output replaces the source while retaining its ID.
        std::fs::write(&target, &bytes)?;
        crate::content::recover(&state).await?;
        assert!(!source.exists());
        assert!(!journal.exists());
        assert_eq!(std::fs::read(&target)?, bytes);
        let file = storage.get_file(i64::try_from(id)?).await?.unwrap();
        assert_eq!(file.path, "Demo.nsz");
        assert_eq!(state.catalog.read().await.files().len(), 1);
        assert_eq!(storage.list_libraries().await?.len(), 1);
        crate::content::recover(&state).await?;
        Ok(())
    }

    #[tokio::test]
    async fn scan_parent_waits_for_children_and_cancel_reaches_descendants() -> Result<()> {
        let dir = tempdir()?;
        let mut state = test_app_state(
            Catalog::from_files(Vec::new()),
            dir.path().into(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        let storage = crate::storage::Storage::open(dir.path().join("test.db")).await?;
        state.storage = Some(storage.clone());
        state.settings.write().await.library.paths = vec![dir.path().to_path_buf()];
        let parent =
            crate::tasks::enqueue(&storage, "scan_libraries", serde_json::json!({})).await?;
        let parent_id = parent["id"].as_str().unwrap().parse::<i64>()?;
        crate::tasks::start(state).await?;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let parent = crate::tasks::get(&storage, parent_id).await?.unwrap();
                if parent["status"] == "COMPLETED" {
                    break;
                }
                assert_ne!(parent["status"], "FAILED", "{parent}");
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let children = crate::tasks::list(&storage).await?;
        let parent_key = parent_id.to_string();
        assert!(
            children
                .iter()
                .any(|task| task["parentId"] == parent_key && task["status"] == "COMPLETED")
        );
        let (parent, child) = storage.with_connection(|conn| {
            conn.execute("INSERT INTO tasks(task_name,input_json,run_after) VALUES('scan_libraries','{}','2999-01-01')", [])?;
            let parent = conn.last_insert_rowid();
            conn.execute("INSERT INTO tasks(task_name,input_json,parent_id,run_after) VALUES('scan_library','{}',?1,'2999-01-01')", [parent])?;
            Ok((parent,conn.last_insert_rowid()))
        }).await?;
        assert!(crate::tasks::cancel(&storage, parent).await?);
        assert!(crate::tasks::cancelled(&storage, child).await);
        Ok(())
    }

    #[tokio::test]
    async fn graphql_tasks_persist_deduplicate_and_cancel() -> Result<()> {
        let dir = tempdir()?;
        let mut state = test_app_state(
            Catalog::from_files(Vec::new()),
            dir.path().into(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        let storage = crate::storage::Storage::open(dir.path().join("test.db")).await?;
        state.storage = Some(storage.clone());
        let server = TestServer::new(router(state))?;
        let request = serde_json::json!({"query":"mutation { enqueueTask(name: \"process_library\") { id status } }"});
        let first = server.post("/api/graphql").json(&request).await.json::<Value>();
        assert!(first.get("errors").is_none(), "{first}");
        let second = server.post("/api/graphql").json(&request).await.json::<Value>();
        assert_eq!(first, second);
        let id = first["data"]["enqueueTask"]["id"].as_str().unwrap();
        assert_eq!(crate::tasks::list(&storage).await?.len(), 1);
        let cancel=server.post("/api/graphql").json(&serde_json::json!({"query":"mutation($id:ID!){cancelTask(id:$id)}","variables":{"id":id}})).await.json::<Value>();
        assert_eq!(cancel["data"]["cancelTask"], true);
        Ok(())
    }

    #[tokio::test]
    async fn settings_parity_shop_patches_preserve_secrets_and_omitted_fields() -> Result<()> {
        let dir = tempdir()?;
        let mut state = test_app_state(
            Catalog::from_files(Vec::new()),
            dir.path().into(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        state.settings_path = dir.path().join("settings.yaml");
        let mut original = state.settings.read().await.clone();
        original.shop.host = "shop.local".into();
        original.shop.clients.tinfoil.clientCertKey = "test-private-key".into();
        original.shop.clients.tinfoil.clientCertPub = "test-public-key".into();
        original.shop.clients.tinfoil.hauth.insert("shop.local".into(), "test-tinfoil".into());
        original.shop.clients.cyberfoil.hauth.insert("shop.local".into(), "test-cyberfoil".into());
        original.shop.clients.cyberfoil.enabled = false;
        original.shop.clients.sphaira.enabled = false;
        *state.settings.write().await = original.clone();
        let server = TestServer::new(router(state.clone()))?;
        let redacted = server.get("/api/settings").await.json::<Value>();
        assert_eq!(redacted["shop"]["clients"]["tinfoil"]["clientCertKey"], "");
        assert_eq!(redacted["shop"]["clients"]["tinfoil"]["hauth"], serde_json::json!({}));
        assert_eq!(redacted["shop"]["clients"]["cyberfoil"]["hauth"], serde_json::json!({}));
        let response = server.post("/api/settings/shop").json(&redacted["shop"]).await;
        assert_eq!(response.json::<Value>()["success"], true);
        assert_eq!(*state.settings.read().await, original);
        let patch = serde_json::json!({
            "host": "https://new.local", "motd": "Updated", "public": true,
            "clients": {
                "tinfoil": {"encrypt": false, "hauth": {"new.local": "ignored"}, "clientCertKey": "ignored"},
                "cyberfoil": {"enabled": true, "hauth": null}
            }
        });
        let response = server.post("/api/settings/shop").json(&patch).await;
        assert_eq!(response.status_code(), StatusCode::OK);
        assert_eq!(response.json::<Value>()["success"], true);
        original.shop.host = "new.local".into();
        original.shop.motd = "Updated".into();
        original.shop.public = true;
        original.shop.clients.tinfoil.encrypt = false;
        original.shop.clients.cyberfoil.enabled = true;
        assert_eq!(*state.settings.read().await, original);
        assert_eq!(crate::settings::Settings::load(&state.settings_path)?, original);
        assert_eq!(state.shop.read().await.motd, "Updated");
        assert!(!state.shop.read().await.encrypt);
        let response = server.post("/api/settings/shop").json(&serde_json::json!({})).await;
        assert_eq!(response.json::<Value>()["success"], true);
        assert_eq!(*state.settings.read().await, original);
        Ok(())
    }

    #[tokio::test]
    async fn settings_parity_shop_rejects_invalid_patches_and_save_failure() -> Result<()> {
        let dir = tempdir()?;
        let mut state = test_app_state(
            Catalog::from_files(Vec::new()),
            dir.path().into(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        state.settings_path = dir.path().join("settings.yaml");
        let original = state.settings.read().await.clone();
        original.save(&state.settings_path)?;
        let persisted = std::fs::read(&state.settings_path)?;
        let motd = state.shop.read().await.motd.clone();
        let encrypt = state.shop.read().await.encrypt;
        let server = TestServer::new(router(state.clone()))?;
        for patch in [
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({"motd": false}),
            serde_json::json!({"public": null}),
            serde_json::json!({"clients": null}),
            serde_json::json!({"clients": {"tinfoil": false}}),
            serde_json::json!({"clients": {"sphaira": {"enabled": "false"}}}),
        ] {
            let response = server.post("/api/settings/shop").json(&patch).await;
            assert_eq!(response.json::<Value>()["success"], false);
            assert_eq!(*state.settings.read().await, original);
            assert_eq!(std::fs::read(&state.settings_path)?, persisted);
        }
        std::fs::create_dir(state.settings_path.with_extension("yaml.tmp"))?;
        let response = server
            .post("/api/settings/shop")
            .json(&serde_json::json!({"motd": "Not published", "clients": {"tinfoil": {"encrypt": !encrypt}}}))
            .await;
        assert_eq!(response.status_code(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(*state.settings.read().await, original);
        assert_eq!(std::fs::read(&state.settings_path)?, persisted);
        assert_eq!(state.shop.read().await.motd, motd);
        assert_eq!(state.shop.read().await.encrypt, encrypt);
        Ok(())
    }

    #[tokio::test]
    async fn login_parity_alias_next_remember_and_sessions() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(vec![AuthUser {
                username: "admin".into(),
                password: "secret".into(),
            }]),
            SessionStore::new(24),
        );
        let existing = state.sessions.create("admin".into());
        let server = TestServer::new(router(state.clone()))?;
        for (route, name, remember) in [("/login", "user", "on"), ("/admin/login", "username", "")]
        {
            let response = server
                .post(route)
                .form(&[
                    (name, "admin"),
                    ("password", "secret"),
                    ("next", "/settings?tab=shop"),
                    ("remember", remember),
                ])
                .await;
            assert_eq!(response.status_code(), StatusCode::SEE_OTHER);
            assert_eq!(response.header("location"), "/settings?tab=shop");
            let cookie = response.header("set-cookie");
            let cookie = cookie.to_str()?;
            for attribute in ["HttpOnly", "Secure", "SameSite=Lax", "Path=/"] {
                assert!(cookie.contains(attribute), "{attribute}");
            }
            assert_eq!(cookie.contains("Max-Age=86400"), !remember.is_empty());
            let cookie = cookie.split(';').next().unwrap();
            let token = cookie.strip_prefix("ownfoil_session=").unwrap();
            assert_eq!(state.sessions.get(token).as_deref(), Some("admin"));
            assert_eq!(state.sessions.get(&existing).as_deref(), Some("admin"));
            let page = server.get("/login?next=%2Fsettings").add_header("cookie", cookie).await;
            assert_eq!(page.header("location"), "/settings");
            server.get("/logout").add_header("cookie", cookie).await;
            assert!(state.sessions.get(token).is_none());
        }
        let page = server.get("/login?next=%2Fsettings%3Ftab%3Dshop%26edit%3D1").await;
        assert!(page.text().contains("name=\"next\" value=\"/settings?tab=shop&amp;edit=1\""));
        let failed = server.post("/login").form(&[("user", "admin"), ("password", "wrong")]).await;
        assert_eq!(failed.header("location"), "/admin/login?error=1");
        assert!(failed.headers().get("set-cookie").is_none());
        let forbidden = server
            .post("/login")
            .add_header("host", "shop.local")
            .add_header("origin", "https://other.local")
            .form(&[("user", "admin"), ("password", "secret")])
            .await;
        assert_eq!(forbidden.status_code(), StatusCode::FORBIDDEN);
        assert!(forbidden.headers().get("set-cookie").is_none());
        Ok(())
    }

    #[tokio::test]
    async fn login_parity_rejects_nonlocal_next() -> Result<()> {
        let state = test_app_state(
            Catalog::from_files(Vec::new()),
            std::env::temp_dir(),
            AuthSettings::from_users(vec![AuthUser {
                username: "admin".into(),
                password: "secret".into(),
            }]),
            SessionStore::new(24),
        );
        let server = TestServer::new(router(state))?;
        for next in [
            "",
            "https://other.local",
            "//other.local",
            "/\\other.local",
            "/%2fother.local",
            "/%255cother.local",
            "/\r\nother",
            "relative",
        ] {
            let response = server
                .post("/login")
                .form(&[("user", "admin"), ("password", "secret"), ("next", next)])
                .await;
            assert_eq!(response.status_code(), StatusCode::SEE_OTHER);
            assert_eq!(response.header("location"), "/admin");
        }
        Ok(())
    }

    #[test]
    fn settings_parity_ui_checks_application_failure_and_posts_only_editable_shop_fields() {
        let html = include_str!("settings.html");
        let api = html.lines().find(|line| line.starts_with("const api=")).unwrap();
        assert!(api.contains("if(d?.success===false)throw new Error"));
        assert!(api.contains("'Operation failed'"));
        let shop = html.lines().find(|line| line.starts_with("submit($('#shop')")).unwrap();
        assert!(!shop.contains("settings.shop"));
        assert!(!shop.contains("hauth"));
        assert!(!shop.contains("clientCertKey"));
        for selector in ["paths", "users"] {
            let handler = html
                .lines()
                .find(|line| line.starts_with(&format!("$('#{selector}').onclick=")))
                .unwrap();
            assert!(handler.find("await api(").unwrap() < handler.find("status('").unwrap());
            assert!(handler.contains("finally{e.target.disabled=false}"));
        }
    }
}
