/// Contract tests built from the released Sphaira client's requests.
mod native_tests {
    use super::*;
    use crate::catalog::IdentifiedContent;
    use axum::http::Method;
    use serde_json::json;

    const BASE: &str = "0100000000010000";
    const UPDATE: &str = "0100000000010800";
    const DLC: &str = "0100000000011001";

    #[tokio::test]
    async fn handshake_identity_access_and_legacy_independence() -> Result<()> {
        let directory = tempdir()?;
        let auth = AuthSettings::from_users(vec![AuthUser {
            username: "admin".into(),
            password: "secret".into(),
        }]);
        auth.upsert_hashed_user(
            "noshop".into(),
            crate::auth::hash_password("secret")?,
            crate::auth::AuthRoles { admin_access: false, shop_access: false, backup_access: true },
        );
        let state = test_app_state(
            Catalog::from_files(vec![]),
            directory.path().into(),
            auth,
            SessionStore::new(24),
        );
        let server = TestServer::new(router(state.clone()))?;
        let private = server.method(Method::OPTIONS, "/").await;
        private.assert_status_unauthorized();
        assert!(private.header("www-authenticate").to_str()?.starts_with("Basic "));
        server
            .method(Method::OPTIONS, "/")
            .add_header("authorization", "Basic bm9zaG9wOnNlY3JldA==")
            .await
            .assert_status_forbidden();
        server
            .method(Method::OPTIONS, "/")
            .add_header("authorization", "Basic YWRtaW46d3Jvbmc=")
            .await
            .assert_status_unauthorized();
        let reply = server
            .method(Method::OPTIONS, "/")
            .add_header("authorization", "Basic YWRtaW46c2VjcmV0")
            .await
            .json::<Value>();
        assert_eq!(reply["protocol_version"], 1);
        assert_eq!(reply["features"]["resumable_download"], true);
        assert_eq!(reply["features"]["save_backup"], false);
        {
            let mut settings = state.settings.write().await;
            settings.shop.public = true;
            settings.shop.name = "My shop".into();
            settings.shop.clients.sphaira.enabled = false;
            settings.shop.clients.tinfoil.enabled = false;
            settings.shop.clients.cyberfoil.enabled = false;
            settings.save(&directory.path().join("settings.yaml"))?;
            let reloaded =
                crate::settings::Settings::load(&directory.path().join("settings.yaml"))?;
            assert_eq!(settings.server.uid, reloaded.server.uid);
            assert_eq!(reply["uid"], crate::discovery::payload(&reloaded, 1234)["uid"]);
        }
        let public = server.method(Method::OPTIONS, "/").await.json::<Value>();
        assert_eq!(public["name"], "My shop");
        assert_eq!(public["uid"], reply["uid"]);
        server.method(Method::OPTIONS, "/base").await.assert_status(StatusCode::METHOD_NOT_ALLOWED);
        let query = json!({"query":"{ files { total } apps { total } }"});
        server.post("/api/graphql").json(&query).await.assert_status_ok();
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn released_sphaira_queries_select_owned_content_and_resume_multi_library_downloads()
    -> Result<()> {
        let directory = tempdir()?;
        let primary = directory.path().join("primary");
        let secondary = directory.path().join("secondary");
        fs::create_dir_all(&primary).await?;
        fs::create_dir_all(&secondary).await?;
        fs::write(primary.join("bundle.xci"), b"0123456789").await?;
        fs::write(secondary.join("base.nsz"), b"compressed-copy").await?;
        let storage = crate::storage::Storage::open(directory.path().join("shop.db")).await?;
        let bundle = ContentFile {
            id: 0,
            library_root: primary.clone(),
            relative_path: "bundle.xci".into(),
            name: "bundle.xci".into(),
            size: 10,
            title_id: Some(BASE.into()),
            version: Some(0),
            kind: ContentKind::Base,
            identified_contents: vec![
                IdentifiedContent {
                    title_id: BASE.into(),
                    app_id: BASE.into(),
                    version: 0,
                    kind: ContentKind::Base,
                },
                IdentifiedContent {
                    title_id: BASE.into(),
                    app_id: UPDATE.into(),
                    version: 65536,
                    kind: ContentKind::Update,
                },
                IdentifiedContent {
                    title_id: BASE.into(),
                    app_id: DLC.into(),
                    version: 0,
                    kind: ContentKind::Dlc,
                },
            ],
        };
        let copy = ContentFile {
            id: 0,
            library_root: secondary.clone(),
            relative_path: "base.nsz".into(),
            name: "base.nsz".into(),
            size: 15,
            title_id: Some(BASE.into()),
            version: Some(0),
            kind: ContentKind::Base,
            identified_contents: vec![],
        };
        let mut files = storage.reconcile_library_scan(primary.clone(), vec![bundle]).await?;
        files.extend(storage.reconcile_library_scan(secondary, vec![copy]).await?);
        storage.with_connection(|conn| {
            conn.execute("INSERT INTO apps(title_id,app_id,app_version,app_type,owned) SELECT id,?1,'131072','UPDATE',0 FROM titles WHERE title_id=?2",[UPDATE,BASE])?;
            conn.execute("UPDATE apps SET display_version='1.1.0' WHERE app_id=?1 AND owned=1",[UPDATE])?;
            Ok(())
        }).await?;
        let mut state = test_app_state(
            Catalog::from_files(files.clone()),
            primary.clone(),
            AuthSettings::from_users(vec![AuthUser {
                username: "admin".into(),
                password: "secret".into(),
            }]),
            SessionStore::new(24),
        );
        state.data_dir = directory.path().join("cache");
        state.storage = Some(storage.clone());
        state.settings.write().await.shop.public = true;
        // A real shop has a small library against a much larger global TitleDB.
        state.titledb = TitleDb::from_entries((0..1000).map(|index| {
            (format!("0200000000{index:06X}"), crate::titledb::TitleInfo {
                name: Some(format!("Unrelated title {index}")),
                ..Default::default()
            })
        }).chain([BASE, DLC].map(|id| (id.into(), crate::titledb::TitleInfo {
            name: Some("Original catalog name".into()),
            icon_url: Some("https://example.test/original.jpg".into()),
            ..Default::default()
        }))));
        state.titledb.set_override(BASE,Some(&json!({"name":"Game","publisher":"Publisher","iconUrl":"https://example.test/icon.jpg","bannerUrl":"https://example.test/banner.jpg","screenshots":["https://example.test/screen.jpg"]}))).await?;
        state
            .titledb
            .set_override(DLC, Some(&json!({"name":"Extra content","intro":"DLC intro"})))
            .await?;
        for id in [BASE, DLC] {
            let record = state.titledb.lookup(id).await.context("Missing test metadata")?.record;
            storage
                .with_connection(move |conn| {
                    conn.execute(
                        "INSERT INTO title_overrides(title_id,record) VALUES(?1,?2)",
                        rusqlite::params![id, serde_json::to_string(&record).unwrap()],
                    )?;
                    Ok(())
                })
                .await?;
        }
        let full = super::super::super::graph_data::GraphData::load(&state, true).await?;
        let scoped = super::super::super::graph_data::GraphData::load_apps(&state, true).await?;
        assert!(full.titles.len() >= 1000);
        assert!(scoped.titles.len() < 10);
        assert_eq!(full.apps, scoped.apps);
        for app in &full.apps {
            for key in ["titleId", "appId"] {
                assert_eq!(full.title(&app[key]), scoped.title(&app[key]));
            }
        }
        let server = TestServer::new(router(state.clone()))?;
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/sphaira_native.json"))?;
        let mut title = Value::Null;
        for case in fixture["cases"].as_array().context("Missing queries")? {
            let response = server.post("/api/graphql").json(case).await;
            response.assert_status_ok();
            let body = response.json::<Value>();
            assert!(body.get("errors").is_none(), "{}: {body}", case["name"]);
            // An extra root forces the complete snapshot; app-only results
            // must remain identical, including filters, sorting and artwork.
            let mut complete_case = case.clone();
            complete_case["query"] = case["query"].as_str().context("Missing query")?
                .replacen('{', "{ __typename ", 1).into();
            let mut complete = server.post("/api/graphql").json(&complete_case).await.json::<Value>();
            assert!(complete.get("errors").is_none(), "{}: {complete}", case["name"]);
            complete["data"].as_object_mut().context("Missing complete data")?.remove("__typename");
            assert_eq!(body["data"], complete["data"], "{}", case["name"]);
            if case["name"] == "TITLE_QUERY" {
                title = body["data"]["title"].clone();
            }
            if case["name"] == "UPDATES_QUERY" {
                assert_eq!(body["data"]["apps"]["items"][0]["appVersion"], 65536);
            }
        }
        assert_eq!(title["base"][0]["latestOwnedVersion"]["version"], 65536);
        assert_eq!(title["updates"][0]["displayVersion"], "1.1.0");
        assert_eq!(title["dlc"][0]["titledb"]["name"], "Extra content");
        assert_ne!(title["banner"]["url"], title["fullBanner"]["url"]);
        assert_ne!(title["screenshots"][0]["url"], title["full"][0]["url"]);
        let base_url = title["base"][0]["downloadUrl"].as_str().context("Missing base download")?;
        let dlc_url = title["dlc"][0]["downloadUrl"].as_str().context("Missing DLC download")?;
        assert!(base_url.starts_with("/api/download/"));
        assert_eq!(title["base"][0]["downloadExtension"], "nsz");
        server.get(base_url).await.assert_text("compressed-copy");
        let ranged = server.get(dlc_url).add_header("range", "bytes=2-5").await;
        ranged.assert_status(StatusCode::PARTIAL_CONTENT);
        ranged.assert_text("2345");
        assert_eq!(ranged.header("content-range"), "bytes 2-5/10");
        server
            .get(dlc_url)
            .add_header("range", "bytes=99-")
            .await
            .assert_status(StatusCode::RANGE_NOT_SATISFIABLE);
        server
            .get("/api/download/00000000000000000000000000000000")
            .await
            .assert_status_not_found();
        // Tokens remain stable after reopening storage and rescanning the same file.
        let reopened = crate::storage::Storage::open(directory.path().join("shop.db")).await?;
        reopened.reconcile_library_scan(primary, vec![files[0].clone()]).await?;
        let again = server
            .post("/api/graphql")
            .json(&json!({"query":"{apps(owned:true,appType:[BASE]){items{downloadUrl}}}"}))
            .await
            .json::<Value>();
        assert_eq!(again["data"]["apps"]["items"][0]["downloadUrl"], base_url);
        state.settings.write().await.shop.public = false;
        server.get(base_url).await.assert_status_unauthorized();
        server
            .post("/api/graphql")
            .json(&json!({"query":"{apps{total}}"}))
            .await
            .assert_status_unauthorized();
        Ok(())
    }

    #[tokio::test]
    async fn artwork_is_fetched_resized_cached_and_gated() -> Result<()> {
        let directory = tempdir()?;
        let mut original = Vec::new();
        image::DynamicImage::new_rgb8(800, 450)
            .write_to(&mut std::io::Cursor::new(&mut original), image::ImageFormat::Png)?;
        let bytes = original.clone();
        let upstream = TestServer::builder().http_transport().build(axum::Router::new().route(
            "/image.png",
            axum::routing::get(move || {
                let bytes = bytes.clone();
                async move { bytes }
            }),
        ))?;
        let url = upstream.server_url("/image.png")?.to_string();
        let mut state = test_app_state(
            Catalog::from_files(vec![]),
            directory.path().into(),
            AuthSettings::from_users(vec![AuthUser {
                username: "admin".into(),
                password: "secret".into(),
            }]),
            SessionStore::new(24),
        );
        state.data_dir = directory.path().join("cache");
        state.settings.write().await.shop.public = true;
        state.titledb.set_override(BASE, Some(&json!({"name":"Game","bannerUrl":url}))).await?;
        let storage = crate::storage::Storage::open(directory.path().join("media.db")).await?;
        let record = state.titledb.lookup(BASE).await.context("Missing media metadata")?.record;
        storage
            .with_connection(move |conn| {
                conn.execute(
                    "INSERT INTO title_overrides(title_id,record) VALUES(?1,?2)",
                    rusqlite::params![BASE, serde_json::to_string(&record).unwrap()],
                )?;
                Ok(())
            })
            .await?;
        state.storage = Some(storage);
        let server = TestServer::new(router(state.clone()))?;
        let response=server.post("/api/graphql").json(&json!({"query":format!("{{title(titleId:\"{BASE}\"){{banner(size:THUMB){{url local size}}}}}}") })).await.json::<Value>();
        let image_url =
            response["data"]["title"]["banner"]["url"].as_str().context("Missing artwork")?;
        let image = server.get(image_url).await;
        image.assert_status_ok();
        assert_eq!(image.header("content-type"), "image/png");
        let decoded = image::load_from_memory(image.as_bytes())?;
        assert_eq!((decoded.width(), decoded.height()), (320, 180));
        drop(upstream);
        // No upstream server is needed for subsequent requests.
        server.get(image_url).await.assert_status_ok();
        server
            .get(image_url)
            .add_header("if-none-match", image.header("etag"))
            .await
            .assert_status(StatusCode::NOT_MODIFIED);
        let changed = image_url.replace("thumb", "screen");
        server.get(&changed).await.assert_status_ok();
        state.settings.write().await.shop.public = false;
        server.get(image_url).await.assert_status_unauthorized();
        Ok(())
    }
}
