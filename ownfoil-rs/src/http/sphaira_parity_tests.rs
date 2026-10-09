/// Differential responses captured from the real pinned Ownfoil 2.5.0 schema.
mod sphaira_parity_tests {
    use super::*;
    use crate::catalog::IdentifiedContent;
    use serde_json::json;

    const UPSTREAM: &str = "a9ac7479f7b54cd52731b24947ac0631cb77ba5f";

    fn kind(value: &Value) -> ContentKind {
        match value.as_str() {
            Some("UPDATE") => ContentKind::Update,
            Some("DLC") => ContentKind::Dlc,
            _ => ContentKind::Base,
        }
    }

    async fn scaling_state(count: usize) -> Result<(tempfile::TempDir, AppState)> {
        let directory = tempdir()?;
        let storage = crate::storage::Storage::open(directory.path().join("scaling.db")).await?;
        storage.with_connection(move |conn| {
            let transaction = conn.transaction()?;
            transaction.execute("INSERT INTO libraries(id,path) VALUES(1,'/parity/games')", [])?;
            for index in 1..=count {
                let id = i64::try_from(index).unwrap();
                let title = format!("010{index:010x}000");
                let filename = format!("Game{index:05}.nsp");
                transaction.execute("INSERT INTO titles(id,title_id,have_base,up_to_date,complete) VALUES(?1,?2,1,1,1)",rusqlite::params![id,title])?;
                transaction.execute("INSERT INTO apps(id,title_id,app_id,app_type,app_version,owned) VALUES(?1,?1,?2,'BASE','0',1)",rusqlite::params![id,title])?;
                transaction.execute("INSERT INTO files(id,library_id,path,folder,name,ext,size,identification_status,identification_type,nb_content) VALUES(?1,1,?2,'/parity/games',?2,'nsp',1024,'identified','cnmt',1)",rusqlite::params![id,filename])?;
                transaction.execute("INSERT INTO app_files(app_id,file_id) VALUES(?1,?1)",[id])?;
            }
            transaction.commit()?;
            Ok(())
        }).await?;
        let files = (1..=count)
            .map(|index| ContentFile {
                id: index,
                library_root: "/parity/games".into(),
                relative_path: format!("Game{index:05}.nsp").into(),
                name: format!("Game{index:05}.nsp"),
                size: 1024,
                title_id: Some(format!("010{index:010x}000")),
                version: Some(0),
                kind: ContentKind::Base,
                identified_contents: Vec::new(),
            })
            .collect();
        let mut state = test_app_state(
            Catalog::from_files(files),
            directory.path().into(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        state.storage = Some(storage);
        state.settings.write().await.shop.public = true;
        state.titledb = TitleDb::from_entries((1..=count).map(|index| {
            let id = format!("010{index:010x}000");
            let name = format!("Game {index:05}");
            (
                id.clone(),
                TitleInfo {
                    name: Some(name.clone()),
                    record: json!({"id":id,"name":name,"publisher":"Synthetic"})
                        .as_object()
                        .unwrap()
                        .clone(),
                    ..Default::default()
                },
            )
        }));
        Ok((directory, state))
    }

    #[tokio::test]
    async fn sphaira_sql_hydrates_only_returned_page_relationships() -> Result<()> {
        let (_directory, state) = scaling_state(100).await?;
        let page = crate::http::graph_data::GraphData::app_page(
            &state,
            false,
            &json!({
                "owned":true,"appType":["BASE"],"groupByAppId":true,"page":2,"pageSize":7,
                "_wantFiles":true,"_wantDownloads":true,"_wantMedia":true
            }),
        )
        .await?;
        assert_eq!(page["total"], 100);
        assert_eq!(page["items"].as_array().unwrap().len(), 7);
        assert_eq!(page["_hydratedCounts"], json!({"titles":7,"apps":7,"files":7}));
        Ok(())
    }

    #[tokio::test]
    #[ignore = "Local debug-build scaling measurement; writes an explicit JSON report"]
    async fn sphaira_catalog_scaling_benchmark() -> Result<()> {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/sphaira_native.json"))?;
        let budget_us = std::env::var("OWNFOIL_BENCHMARK_P95_BUDGET_US")
            .map_or(Ok(500_000_u64), |value| value.parse())?;
        let mut report = Vec::new();
        for count in [100, 10_000] {
            let (_directory, state) = scaling_state(count).await?;
            let provider_started = std::time::Instant::now();
            let scoped = crate::http::graph_data::GraphData::app_page(
                &state,
                false,
                &json!({
                    "owned":true,"appType":["BASE"],"groupByAppId":true,"pageSize":40,
                    "_wantFiles":false,"_wantDownloads":false,"_wantMedia":true
                }),
            )
            .await?;
            let provider_us = u64::try_from(provider_started.elapsed().as_micros())?;
            assert_eq!(scoped["_hydratedCounts"], json!({"titles":40,"apps":40,"files":0}));
            let server = TestServer::new(
                axum::Router::new()
                    .route("/api/graphql", axum::routing::post(crate::http::graphql::post))
                    .with_state(state.clone()),
            )?;
            for case in fixture["cases"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|case| case["name"].as_str().unwrap().starts_with("all_"))
            {
                let mut samples = Vec::new();
                let request = json!({"query":case["query"],"variables":case["variables"]});
                let mut first = None;
                for index in 0..21 {
                    let started = std::time::Instant::now();
                    let response = server.post("/api/graphql").json(&request).await;
                    let elapsed = u64::try_from(started.elapsed().as_micros())?;
                    response.assert_status_ok();
                    let data = response.json::<Value>();
                    assert!(data["errors"].is_null(), "{data}");
                    assert_eq!(data["data"]["apps"]["total"], count);
                    assert_eq!(data["data"]["apps"]["items"].as_array().unwrap().len(), 40);
                    if index == 0 { first = Some(elapsed) } else { samples.push(elapsed) }
                }
                samples.sort_unstable();
                assert!(
                    samples[18] <= budget_us,
                    "{} {count} titles p95 {}us exceeds local budget {budget_us}us",
                    case["name"],
                    samples[18]
                );
                report.push(json!({"library_titles":count,"query":case["name"],
                    "first_us":first,"warm_p50_us":samples[9].midpoint(samples[10]),
                    "warm_p95_us":samples[18],"samples":20,"returned_apps":40,
                    "hydrated_counts":scoped["_hydratedCounts"],"provider_sync_and_first_scope_us":provider_us}));
            }
        }
        let output = std::env::var_os("OWNFOIL_BENCHMARK_OUTPUT").map_or_else(
            || std::env::temp_dir().join("ownfoil-sphaira-scaling.json"),
            PathBuf::from,
        );
        fs::write(output, serde_json::to_vec_pretty(&json!({"mode":"Rust debug build, in-process Axum test transport, sequential requests",
            "architecture":std::env::consts::ARCH,"os":std::env::consts::OS,"warm_p95_budget_us":budget_us,
            "baseline":"current branch; no pre-change or Python comparison","cases":report}))?).await?;
        Ok(())
    }

    #[test]
    fn sphaira_250_artwork_geometry_and_settings_defaults() -> Result<()> {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/sphaira_250_parity.json"))?;
        let defaults = crate::settings::Settings::default();
        assert_eq!(defaults.local_media.enabled, fixture["behavior"]["local_media_default"]);
        assert_eq!(
            defaults.library.management.deduplication.prefer_multicontent,
            fixture["behavior"]["prefer_multicontent_default"]
        );
        for case in fixture["behavior"]["fit_examples"].as_array().unwrap() {
            let width = u32::try_from(case["original"][0].as_u64().unwrap())?;
            let height = u32::try_from(case["original"][1].as_u64().unwrap())?;
            let (width, height) = crate::media::fit(
                width,
                height,
                case["kind"].as_str().unwrap(),
                case["size"].as_str().unwrap(),
            );
            assert_eq!(json!([width, height]), case["result"], "{case}");
        }
        Ok(())
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn sphaira_matches_250_released_client_response_matrix() -> Result<()> {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/sphaira_250_parity.json"))?;
        assert_eq!(fixture["upstream"], UPSTREAM);
        assert_eq!(
            fixture["behavior"]["metadata_priority"],
            json!(["custom", "titledb", "extract"])
        );
        let directory = tempdir()?;
        let storage = crate::storage::Storage::open(directory.path().join("parity-250.db")).await?;
        let seed = fixture.clone();
        storage.with_connection(move |conn| {
            conn.execute("INSERT INTO libraries(id,path) VALUES(1,'/parity/games'),(2,'/parity/secondary')", [])?;
            for title in seed["titles"].as_array().unwrap() {
                conn.execute("INSERT INTO titles(id,title_id,have_base,up_to_date,complete) VALUES(?1,?2,?3,?4,?5)", rusqlite::params![title["id"].as_i64(),title["title_id"].as_str(),title["have_base"].as_bool(),title["up_to_date"].as_bool(),title["complete"].as_bool()])?;
            }
            for app in seed["apps"].as_array().unwrap() {
                conn.execute("INSERT INTO apps(id,title_id,app_id,app_type,app_version,owned,display_version) VALUES(?1,?2,?3,?4,?5,?6,?7)", rusqlite::params![app["id"].as_i64(),app["title_id"].as_i64(),app["app_id"].as_str(),app["app_type"].as_str(),app["app_version"].as_str(),app["owned"].as_bool(),app["display_version"].as_str()])?;
            }
            for file in seed["files"].as_array().unwrap() {
                let folder = if file["library_id"] == 2 { "/parity/secondary" } else { "/parity/games" };
                conn.execute("INSERT INTO files(id,library_id,path,folder,name,ext,size,identification_status,identification_type,nb_content,organized,added_at,download_token) VALUES(?1,?2,?3,?4,?3,?5,?6,'identified',?7,?8,?9,?10,?11)", rusqlite::params![file["id"].as_i64(),file["library_id"].as_i64(),file["filename"].as_str(),folder,file["extension"].as_str(),file["size"].as_i64(),file["identification_type"].as_str(),if file["multicontent"] == true {2} else {1},file["organized"].as_bool(),file["added_at"].as_str(),file["download_token"].as_str()])?;
                for app_id in file["apps"].as_array().unwrap() {
                    conn.execute("INSERT INTO app_files(app_id,file_id) VALUES(?1,?2)", rusqlite::params![app_id.as_i64(),file["id"].as_i64()])?;
                }
            }
            Ok(())
        }).await?;
        let files = fixture["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|file| {
                let contents = file["apps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|id| {
                        let app = fixture["apps"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|app| app["id"] == *id)
                            .unwrap();
                        let title = fixture["titles"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|title| title["id"] == app["title_id"])
                            .unwrap();
                        IdentifiedContent {
                            title_id: title["title_id"].as_str().unwrap().into(),
                            app_id: app["app_id"].as_str().unwrap().into(),
                            version: app["app_version"].as_str().unwrap().parse().unwrap(),
                            kind: kind(&app["app_type"]),
                        }
                    })
                    .collect::<Vec<_>>();
                ContentFile {
                    id: file["id"].as_u64().unwrap().try_into().unwrap(),
                    library_root: if file["library_id"] == 2 {
                        "/parity/secondary".into()
                    } else {
                        "/parity/games".into()
                    },
                    relative_path: file["filename"].as_str().unwrap().into(),
                    name: file["filename"].as_str().unwrap().into(),
                    size: file["size"].as_u64().unwrap(),
                    title_id: Some(contents[0].app_id.clone()),
                    version: Some(contents[0].version),
                    kind: contents[0].kind,
                    identified_contents: contents,
                }
            })
            .collect();
        let mut state = test_app_state(
            Catalog::from_files(files),
            directory.path().into(),
            AuthSettings::from_users(Vec::new()),
            SessionStore::new(24),
        );
        state.storage = Some(storage);
        state.settings.write().await.shop.public = true;
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
        let server = TestServer::new(
            axum::Router::new()
                .route("/api/graphql", axum::routing::post(crate::http::graphql::post))
                .with_state(state.clone()),
        )?;
        for case in fixture["cases"].as_array().unwrap() {
            state.settings.write().await.library.management.deduplication.prefer_multicontent =
                case["prefer_multicontent"].as_bool().unwrap();
            let response = server
                .post("/api/graphql")
                .json(&json!({
                    "query": case["query"], "variables": case["variables"]
                }))
                .await;
            response.assert_status_ok();
            let actual = response.json::<Value>();
            assert_eq!(
                actual["data"],
                case["data"],
                "{} prefer={} page={} errors={}",
                case["name"],
                case["prefer_multicontent"],
                case["variables"]["page"],
                actual["errors"]
            );
        }
        Ok(())
    }
}
