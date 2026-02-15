#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::module_inception)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::Result;
    use axum::http::StatusCode;
    use axum_test::TestServer;
    use serde_json::Value;
    use tempfile::tempdir;
    use tokio::fs;
    use tokio::sync::RwLock;

    use crate::auth::{AuthSettings, AuthUser};
    use crate::catalog::{Catalog, ContentFile, ContentKind};
    use crate::config::TitleDbConfig;
    use crate::titledb::TitleDb;

    use crate::http::{router, state::SessionStore, AppState};

    fn test_app_state(
        catalog: Catalog,
        library_root: PathBuf,
        auth: AuthSettings,
        sessions: SessionStore,
    ) -> AppState {
        let data_dir = std::env::temp_dir().join("ownfoil-test");
        let (progress_tx, _) = tokio::sync::broadcast::channel(1);
        let titledb = TitleDb::with_progress(
            TitleDbConfig {
                enabled: false,
                ..Default::default()
            },
            data_dir.clone(),
            Some(progress_tx.clone()),
        );
        AppState {
            catalog: Arc::new(RwLock::new(catalog)),
            library_root,
            auth: Arc::new(auth),
            sessions,
            titledb,
            data_dir,
            titledb_progress_tx: progress_tx,
        }
    }

    #[tokio::test]
    async fn health_returns_ok_with_catalog_count() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("a.nsp"),
            name: String::from("a.nsp"),
            size: 1,
            title_id: None,
            version: None,
            kind: ContentKind::Unknown,
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
        assert_eq!(
            body.get("catalog_files"),
            Some(&Value::Number(1_i64.into()))
        );
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

        let response = server
            .get("/api/download/demo.nsp")
            .add_header("Range", "bytes=1-3")
            .await;

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
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
        }]);

        let state = test_app_state(
            catalog,
            dir.path().to_path_buf(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );

        let server = TestServer::new(router(state))?;
        let response = server
            .get("/api/get_game/1")
            .add_header("Range", "bytes=1-3")
            .await;

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
        assert_eq!(
            unauthorized.header("www-authenticate"),
            "Basic realm=\"ownfoil-rs\""
        );

        let authorized = server
            .get("/api/catalog")
            .add_header("Authorization", "Basic YWRtaW46d3Jvbmc=")
            .await;
        assert_eq!(authorized.status_code(), StatusCode::UNAUTHORIZED);

        let authorized = server
            .get("/api/catalog")
            .add_header("Authorization", "Basic YWRtaW46c2VjcmV0")
            .await;
        assert_eq!(authorized.status_code(), StatusCode::OK);

        let authorized = server
            .get("/api/catalog")
            .add_header("Authorization", "YWRtaW46c2VjcmV0")
            .await;
        assert_eq!(authorized.status_code(), StatusCode::OK);
        Ok(())
    }

    #[tokio::test]
    async fn shop_response_contains_files_list() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
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
        let files = body
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(files.len(), 1);
        let first = files[0].as_object().cloned().unwrap_or_default();
        assert_eq!(
            first.get("url"),
            Some(&Value::String(String::from("/api/get_game/1#demo.nsp")))
        );
        assert_eq!(first.get("size"), Some(&Value::Number(10_u64.into())));
        assert_eq!(
            body.get("success"),
            Some(&Value::String(String::from("ok")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn root_response_contains_files_list() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
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
        let files = body
            .get("files")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
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
            relative_path: PathBuf::from("demo.nsp"),
            name: String::from("demo.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000000")),
            version: Some(0),
            kind: ContentKind::Base,
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
        let sections = body
            .get("sections")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(sections.len(), 5);

        let first_section = sections
            .first()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        assert_eq!(
            first_section.get("id"),
            Some(&Value::String(String::from("new")))
        );
        let items = first_section
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert_eq!(items.len(), 1);

        let first_item = items[0].as_object().cloned().unwrap_or_default();
        assert_eq!(
            first_item.get("url"),
            Some(&Value::String(String::from("/api/get_game/1#demo.nsp")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn shop_sections_new_falls_back_to_all_when_no_base_items() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("update.nsp"),
            name: String::from("update.nsp"),
            size: 10,
            title_id: Some(String::from("0100000000000800")),
            version: Some(65536),
            kind: ContentKind::Update,
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
        let sections = body
            .get("sections")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
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
            relative_path: PathBuf::from("update.nsp"),
            name: String::from("update.nsp"),
            size: 10,
            title_id: Some(String::from("0100ABCD12340800")),
            version: Some(65536),
            kind: ContentKind::Update,
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
        assert_eq!(
            item.get("title_id"),
            Some(&Value::String(String::from("0100ABCD12340000")))
        );
        assert_eq!(
            item.get("app_id"),
            Some(&Value::String(String::from("0100ABCD12340800")))
        );
        assert_eq!(
            item.get("app_type"),
            Some(&Value::String(String::from("UPDATE")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn dlc_section_item_uses_base_title_id_and_dlc_app_id() -> Result<()> {
        let catalog = Catalog::from_files(vec![ContentFile {
            relative_path: PathBuf::from("dlc.nsp"),
            name: String::from("dlc.nsp"),
            size: 10,
            title_id: Some(String::from("0100ABCD12341001")),
            version: Some(0),
            kind: ContentKind::Dlc,
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
        assert_eq!(
            item.get("title_id"),
            Some(&Value::String(String::from("0100ABCD12340000")))
        );
        assert_eq!(
            item.get("app_id"),
            Some(&Value::String(String::from("0100ABCD12341001")))
        );
        assert_eq!(
            item.get("app_type"),
            Some(&Value::String(String::from("DLC")))
        );
        Ok(())
    }

    #[tokio::test]
    async fn updates_section_keeps_only_latest_version_per_base_title() -> Result<()> {
        let catalog = Catalog::from_files(vec![
            ContentFile {
                relative_path: PathBuf::from("update-old.nsp"),
                name: String::from("update-old.nsp"),
                size: 10,
                title_id: Some(String::from("0100ABCD12340800")),
                version: Some(65536),
                kind: ContentKind::Update,
            },
            ContentFile {
                relative_path: PathBuf::from("update-new.nsp"),
                name: String::from("update-new.nsp"),
                size: 10,
                title_id: Some(String::from("0100ABCD12340800")),
                version: Some(131072),
                kind: ContentKind::Update,
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
        assert_eq!(
            item.get("app_version"),
            Some(&Value::String(String::from("131072")))
        );
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
        assert_eq!(
            response.header("cache-control"),
            "public, max-age=604800, immutable"
        );
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
        assert_eq!(
            body.get("saves").and_then(Value::as_array).map(Vec::len),
            Some(0)
        );
        Ok(())
    }
}
