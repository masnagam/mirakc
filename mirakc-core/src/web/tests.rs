use super::*;

use std::future::Future;

use assert_matches::assert_matches;
use axum_test_helper::TestClient;
use axum_test_helper::TestResponse;
use maplit::hashmap;

use crate::broadcaster::BroadcasterStream;
use crate::config::FilterConfig;
use crate::config::MountConfig;
use crate::config::PostFilterConfig;
use crate::epg::EpgProgram;
use crate::epg::EpgService;
use crate::epg::RemoveAirtimeMessage;
use crate::timeshift::TimeshiftLiveStreamSource;
use crate::timeshift::TimeshiftRecordStreamSource;
use crate::tuner::StopStreamingMessage;
use crate::tuner::TunerSubscriptionId;

#[actix::test]
async fn test_get_unknown() {
    let res = get("/api/unknown").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[actix::test]
async fn test_get_version() {
    let res = get("/api/version").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_get_status() {
    let res = get("/api/status").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_get_channels() {
    let res = get("/api/channels").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_get_services() {
    let res = get("/api/services").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_get_service() {
    let res = get("/api/services/1").await;
    assert_eq!(res.status(), StatusCode::OK);

    let res = get("/api/services/0").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[actix::test]
async fn test_get_service_logo() {
    let res = get("/api/services/1/logo").await;
    assert_eq!(res.status(), StatusCode::OK);

    let res = get("/api/services/0/logo").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let res = get("/api/services/2/logo").await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[actix::test]
async fn test_head_service_logo() {
    let res = head("/api/services/1/logo").await;
    assert_eq!(res.status(), StatusCode::OK);

    let res = head("/api/services/0/logo").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let res = head("/api/services/2/logo").await;
    assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[actix::test]
async fn test_get_programs() {
    let res = get("/api/programs").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_get_program() {
    let res = get("/api/programs/1").await;
    assert_eq!(res.status(), StatusCode::OK);

    let res = get("/api/programs/0").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[actix::test]
async fn test_get_tuners() {
    let res = get("/api/tuners").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_get_channel_stream() {
    let res = get("/api/channels/GR/ch/stream").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().contains_key(X_MIRAKURUN_TUNER_USER_ID));
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "video/MP2T");
    });

    let res = get("/api/channels/GR/0/stream").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let res = get("/api/channels/WOWOW/ch/stream").await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let res = get("/api/channels/GR/xx/stream").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let decode_values = [0, 1];

    let valid_pairs = ["GR", "BS", "CS", "SKY"].iter().zip(&decode_values);
    for (channel_type, decode) in valid_pairs {
        let res =
            get(format!("/api/channels/{}/ch/stream?decode={}", channel_type, decode).as_str())
                .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
            assert_eq!(v, "none");
        });
    }

    for decode in &decode_values {
        let res = get(format!("/api/channels/WOWOW/ch/stream?decode={}", decode).as_str()).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    let res = get("/api/channels/GR/ch/stream?post-filters[]=mp4").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "video/mp4");
    });
}

#[actix::test]
async fn test_get_channel_service_stream() {
    let res = get("/api/channels/GR/ch/services/1/stream").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().contains_key(X_MIRAKURUN_TUNER_USER_ID));
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "video/MP2T");
    });
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });

    let res = get("/api/channels/GR/0/services/1/stream").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let res = get("/api/channels/WOWOW/ch/services/1/stream").await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    let decode_values = [0, 1];

    let valid_pairs = ["GR", "BS", "CS", "SKY"].iter().zip(&decode_values);
    for (channel_type, decode) in valid_pairs {
        let res = get(format!(
            "/api/channels/{}/ch/services/1/stream?decode={}",
            channel_type, decode
        )
        .as_str())
        .await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
            assert_eq!(v, "none");
        });
    }

    for decode in &decode_values {
        let res =
            get(format!("/api/channels/WOWOW/ch/services/1/stream?decode={}", decode).as_str())
                .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    for decode in &decode_values {
        let res =
            get(format!("/api/channels/WOWOW/ch/services/2/stream?decode={}", decode).as_str())
                .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    let res = get("/api/channels/GR/ch/services/1/stream?post-filters[]=mp4").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "video/mp4");
    });
}

#[actix::test]
async fn test_get_service_stream() {
    let res = get("/api/services/1/stream").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().contains_key(X_MIRAKURUN_TUNER_USER_ID));
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "video/MP2T");
    });
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });

    let res = get("/api/services/0/stream").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let decode_values = [0, 1];

    for decode in &decode_values {
        let res = get(format!("/api/services/1/stream?decode={}", decode).as_str()).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
                assert_eq!(v, "none");
        });
    }

    for decode in &decode_values {
        let res = get(format!("/api/services/0/stream?decode={}", decode).as_str()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    for decode in &decode_values {
        let res = get(format!("/api/services/2/stream?decode={}", decode).as_str()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    let res = get("/api/services/1/stream?post-filters[]=mp4").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "video/mp4");
    });
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });
}

#[actix::test]
async fn test_head_service_stream() {
    let res = head("/api/services/1/stream").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "video/MP2T");
    });

    let res = head("/api/services/0/stream").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // See comments in head_service_stream().
    let res = head("/api/services/2/stream").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });

    let res = head("/api/services/1/stream?post-filters[]=mp4").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "video/mp4");
    });
}

#[actix::test]
async fn test_get_program_stream() {
    let res = get("/api/programs/100001/stream").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });
    assert!(res.headers().contains_key(X_MIRAKURUN_TUNER_USER_ID));
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "video/MP2T");
    });

    let res = get("/api/programs/0/stream").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let decode_values = [0, 1];

    for decode in &decode_values {
        let res = get(format!("/api/programs/100001/stream?decode={}", decode).as_str()).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
            assert_eq!(v, "none");
        });
    }

    for decode in &decode_values {
        let res = get(format!("/api/programs/0/stream?decode={}", decode).as_str()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    for decode in &decode_values {
        let res = get(format!("/api/programs/200001/stream?decode={}", decode).as_str()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    let res = get("/api/programs/100001/stream?post-filters[]=mp4").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "video/mp4");
    });
}

#[actix::test]
async fn test_get_timeshift_recorders() {
    let res = get("/api/timeshift").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_get_timeshift_recorder() {
    let res = get("/api/timeshift/test").await;
    assert_eq!(res.status(), StatusCode::OK);

    let res = get("/api/timeshift/not_found").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[actix::test]
async fn test_get_timeshift_records() {
    let res = get("/api/timeshift/test/records").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_get_timeshift_record() {
    let res = get("/api/timeshift/test/records/1").await;
    assert_eq!(res.status(), StatusCode::OK);

    let res = get("/api/timeshift/test/records/2").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[actix::test]
async fn test_get_timeshift_stream() {
    let res = get("/api/timeshift/test/stream").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });
    assert!(res.headers().contains_key(X_MIRAKURUN_TUNER_USER_ID));

    let res = get("/api/timeshift/not_found/stream").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[actix::test]
async fn test_get_timeshift_record_stream() {
    let res = get("/api/timeshift/test/records/1/stream").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().contains_key(X_MIRAKURUN_TUNER_USER_ID));
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "bytes");
    });
    assert!(res.headers().contains_key("content-range"));
    assert!(res.headers().contains_key("content-length"));

    let res = get("/api/timeshift/test/records/1/stream?pre-filters[]=cat").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().contains_key(X_MIRAKURUN_TUNER_USER_ID));
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "bytes");
    });
    assert!(res.headers().contains_key("content-range"));
    assert!(res.headers().contains_key("content-length"));

    let res = get("/api/timeshift/test/records/1/stream?post-filters[]=cat").await;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers().contains_key(X_MIRAKURUN_TUNER_USER_ID));
    assert_matches!(res.headers().get(ACCEPT_RANGES), Some(v) => {
        assert_eq!(v, "none");
    });
    assert!(!res.headers().contains_key("content-range"));
    assert!(!res.headers().contains_key("content-length"));

    let res = get("/api/timeshift/not_found/records/1/stream").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[actix::test]
async fn test_get_iptv_playlist() {
    test_get_iptv_playlist_("/api/iptv/playlist").await;
}

#[actix::test]
async fn test_get_iptv_channel_m3u8() {
    test_get_iptv_playlist_("/api/iptv/channel.m3u8").await;
}

async fn test_get_iptv_playlist_(endpoint: &str) {
    let res = get(endpoint).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "application/x-mpegurl; charset=UTF-8");
    });
    let playlist = res.text().await;
    assert!(playlist.contains("#KODIPROP:mimetype=video/mp2t\n"));

    let res = get(&format!("{}?post-filters[]=mp4", endpoint)).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_matches!(res.headers().get("content-type"), Some(v) => {
        assert_eq!(v, "application/x-mpegurl; charset=UTF-8");
    });
    let playlist = res.text().await;
    assert!(playlist.contains("#KODIPROP:mimetype=video/mp4\n"));
}

#[actix::test]
async fn test_get_iptv_epg() {
    let res = get("/api/iptv/epg").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_get_iptv_xmltv() {
    let res = get("/api/iptv/xmltv").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_get_docs() {
    let res = get("/api/docs").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_access_control_localhost() {
    let addr = "127.0.0.1:10000".parse().unwrap();
    let res = get_with_peer_addr("/api/version", Some(addr)).await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_access_control_uds() {
    let res = get_with_peer_addr("/api/version", None).await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_access_control_denied() {
    let addr = "8.8.8.8:10000".parse().unwrap();
    let res = get_with_peer_addr("/api/version", Some(addr)).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[actix::test]
async fn test_do_streaming() {
    let user = user_for_test(0.into());

    let result = do_streaming(
        user.clone(),
        futures::stream::empty(),
        "video/MP2T".to_string(),
        None,
        (),
        1000,
    )
    .await;
    assert_matches!(result, Err(Error::ProgramNotFound));

    let result = do_streaming(
        user.clone(),
        futures::stream::pending(),
        "video/MP2T".to_string(),
        None,
        (),
        1,
    )
    .await;
    assert_matches!(result, Err(Error::StreamingTimedOut));
}

#[actix::test]
async fn test_mount() {
    let res = get("/").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let res = get("/Cargo.toml").await;
    assert_eq!(res.status(), StatusCode::OK);

    let res = get("/no-such-file").await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let res = get("/api/version").await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[actix::test]
async fn test_filter_setting() {
    async fn do_test<H, F>(query: &str, handler: H) -> StatusCode
    where
        H: FnOnce(Qs<FilterSetting>) -> F + Clone + Send + 'static,
        F: Future<Output = ()> + Send,
    {
        let url = format!("/?{}", query);
        let app = Router::new().route("/", routing::get(handler));
        TestClient::new(app).get(&url).send().await.status()
    }

    assert_eq!(
        do_test("", |Qs(v)| async move {
            assert!(v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("unknown=0", |Qs(v)| async move {
            assert!(v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("decode=0", |Qs(v)| async move {
            assert!(!v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("decode=1", |Qs(v)| async move {
            assert!(v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("decode=false", |Qs(v)| async move {
            assert!(!v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("decode=true", |Qs(v)| async move {
            assert!(v.decode);
            assert!(v.pre_filters.is_empty());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("decode=x", |_| async move {}).await,
        StatusCode::BAD_REQUEST
    );

    assert_eq!(
        do_test("pre-filters[]=a", |Qs(v)| async move {
            assert!(v.decode);
            assert_eq!(v.pre_filters.len(), 1);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("pre-filters[]=a&pre-filters[]=b", |Qs(v)| async move {
            assert!(v.decode);
            assert_eq!(v.pre_filters.len(), 2);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert_eq!(v.pre_filters[1], "b".to_string());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("pre-filters[0]=a", |Qs(v)| async move {
            assert!(v.decode);
            assert_eq!(v.pre_filters.len(), 1);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("pre-filters[0]=a&pre-filters[1]=b", |Qs(v)| async move {
            assert!(v.decode);
            assert_eq!(v.pre_filters.len(), 2);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert_eq!(v.pre_filters[1], "b".to_string());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("pre-filters[1]=a", |Qs(v)| async move {
            assert!(v.decode);
            assert_eq!(v.pre_filters.len(), 1);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("pre-filters[1]=a&pre-filters[2]=b", |Qs(v)| async move {
            assert!(v.decode);
            assert_eq!(v.pre_filters.len(), 2);
            assert_eq!(v.pre_filters[0], "a".to_string());
            assert_eq!(v.pre_filters[1], "b".to_string());
            assert!(v.post_filters.is_empty());
        })
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test("pre-filters=a", |_| async move {}).await,
        StatusCode::BAD_REQUEST
    );

    assert_eq!(
        do_test("pre-filters[x]=a", |_| async move {}).await,
        StatusCode::BAD_REQUEST
    );

    assert_eq!(
        do_test("pre-filters[0]=a&pre-filters[0]=b", |_| async move {}).await,
        StatusCode::BAD_REQUEST
    );

    assert_eq!(
        do_test(
            "decode=1&pre-filters[]=a&post-filters[]=b",
            |Qs(v)| async move {
                assert!(v.decode);
                assert_eq!(v.pre_filters.len(), 1);
                assert_eq!(v.pre_filters[0], "a".to_string());
                assert_eq!(v.post_filters.len(), 1);
                assert_eq!(v.post_filters[0], "b".to_string());
            }
        )
        .await,
        StatusCode::OK
    );

    assert_eq!(
        do_test(
            "pre-filters[]=a&decode=1&post-filters[]=b",
            |Qs(v)| async move {
                assert!(v.decode);
                assert_eq!(v.pre_filters.len(), 1);
                assert_eq!(v.pre_filters[0], "a".to_string());
                assert_eq!(v.post_filters.len(), 1);
                assert_eq!(v.post_filters[0], "b".to_string());
            }
        )
        .await,
        StatusCode::OK
    );
}

async fn get_with_peer_addr(url: &str, addr: Option<SocketAddr>) -> TestResponse {
    let app = create_app().layer(helper::ReplaceConnectInfoLayer::new(addr));
    TestClient::new(app).get(url).send().await
}

async fn get(url: &str) -> TestResponse {
    let app = create_app();
    TestClient::new(app).get(url).send().await
}

async fn head(url: &str) -> TestResponse {
    let app = create_app();
    TestClient::new(app).head(url).send().await
}

fn create_app() -> Router {
    let state = Arc::new(AppState {
        config: config_for_test(),
        string_table: string_table_for_test(),
        tuner_manager: tuner_manager_for_test(),
        epg: epg_for_test(),
        timeshift_manager: timeshift_manager_for_test(),
    });
    build_app(state)
}

fn config_for_test() -> Arc<Config> {
    let mut config = Config::default();
    // Map "/" to CARGO_MANIFEST_DIR.
    config.server.mounts.insert(
        "/".to_string(),
        MountConfig {
            path: env!("CARGO_MANIFEST_DIR").into(),
            // TODO: ServeDir doesn't support specifying the index filename.
            index: None,
            // TODO: ServeDir doesn't support listing entries.
            listing: false,
        },
    );
    // Disable service and program filters
    config.filters.service_filter = Default::default();
    config.filters.program_filter = Default::default();
    // filters for testing
    config.pre_filters.insert(
        "cat".to_string(),
        FilterConfig {
            command: "cat".to_string(),
        },
    );
    config.post_filters.insert(
        "cat".to_string(),
        PostFilterConfig {
            command: "cat".to_string(),
            content_type: None,
        },
    );
    config.post_filters.insert(
        "mp4".to_string(),
        PostFilterConfig {
            command: "cat".to_string(),
            content_type: Some("video/mp4".to_string()),
        },
    );
    // Disable tracking airtime
    config.recorder.track_airtime_command = "true".to_string();
    // logo for SID#1
    config.resource.logos = hashmap! {
        ServiceTriple::new(0.into(), 0.into(), 1.into()) =>
            "/dev/null".to_string(),
    };
    // "/dev/null" is enough to test
    config.mirakurun.openapi_json = "/dev/null".to_string();

    Arc::new(config)
}

fn string_table_for_test() -> Arc<StringTable> {
    crate::string_table::load(
        format!("{}/../resources/strings.yml", env!("CARGO_MANIFEST_DIR")).as_str(),
    )
}

fn tuner_manager_for_test() -> Addr<TunerManager> {
    TunerManager::mock(Box::new(|msg, _ctx| {
        if let Some(_) = msg.downcast_ref::<QueryTunersMessage>() {
            Box::<Option<Result<Vec<MirakurunTuner>, Error>>>::new(Some(Ok(Vec::new())))
        } else if let Some(msg) = msg.downcast_ref::<StartStreamingMessage>() {
            if msg.channel.channel == "ch" {
                let (tx, stream) = BroadcasterStream::new_for_test();
                let _ = tx.try_send(Bytes::from("hi"));
                let result = Ok(MpegTsStream::new(TunerSubscriptionId::default(), stream));
                Box::<Option<Result<_, Error>>>::new(Some(result))
            } else {
                let (_, stream) = BroadcasterStream::new_for_test();
                let result = Ok(MpegTsStream::new(TunerSubscriptionId::default(), stream));
                Box::<Option<Result<_, Error>>>::new(Some(result))
            }
        } else if let Some(_) = msg.downcast_ref::<StopStreamingMessage>() {
            Box::new(Some(()))
        } else {
            unimplemented!();
        }
    }))
    .start()
}

fn epg_for_test() -> Addr<Epg> {
    Epg::mock(Box::new(|msg, _| {
        if let Some(_) = msg.downcast_ref::<QueryChannelsMessage>() {
            Box::<Option<Result<Vec<MirakurunChannel>, Error>>>::new(Some(Ok(Vec::new())))
        } else if let Some(msg) = msg.downcast_ref::<QueryChannelMessage>() {
            let result = if msg.channel == "0" {
                Err(Error::ChannelNotFound)
            } else {
                Ok(EpgChannel {
                    name: "test".to_string(),
                    channel_type: msg.channel_type,
                    channel: msg.channel.clone(),
                    extra_args: "".to_string(),
                    services: Vec::new(),
                    excluded_services: Vec::new(),
                })
            };
            Box::<Option<Result<EpgChannel, Error>>>::new(Some(result))
        } else if let Some(_) = msg.downcast_ref::<QueryServicesMessage>() {
            Box::<Option<Result<Vec<EpgService>, Error>>>::new(Some(Ok(vec![EpgService {
                nid: 0.into(),
                tsid: 0.into(),
                sid: 1.into(),
                service_type: 1,
                logo_id: 0,
                remote_control_key_id: 0,
                name: "test".to_string(),
                channel: EpgChannel {
                    name: "test".to_string(),
                    channel_type: ChannelType::GR,
                    channel: "ch".to_string(),
                    extra_args: "".to_string(),
                    services: Vec::new(),
                    excluded_services: Vec::new(),
                },
            }])))
        } else if let Some(msg) = msg.downcast_ref::<QueryServiceMessage>() {
            let result = match msg {
                QueryServiceMessage::ByNidSid { nid, sid } => {
                    if sid.value() == 0 {
                        Err(Error::ServiceNotFound)
                    } else {
                        let channel = if sid.value() == 1 { "ch" } else { "" };
                        Ok(EpgService {
                            nid: *nid,
                            tsid: 0.into(),
                            sid: *sid,
                            service_type: 1,
                            logo_id: 0,
                            remote_control_key_id: 0,
                            name: "test".to_string(),
                            channel: EpgChannel {
                                name: "test".to_string(),
                                channel_type: ChannelType::GR,
                                channel: channel.to_string(),
                                extra_args: "".to_string(),
                                services: Vec::new(),
                                excluded_services: Vec::new(),
                            },
                        })
                    }
                }
            };
            Box::<Option<Result<EpgService, Error>>>::new(Some(result))
        } else if let Some(msg) = msg.downcast_ref::<QueryClockMessage>() {
            let result = match msg.triple.sid().value() {
                0 => Err(Error::ClockNotSynced),
                _ => Ok(Clock {
                    pid: 0,
                    pcr: 0,
                    time: 0,
                }),
            };
            Box::<Option<Result<Clock, Error>>>::new(Some(result))
        } else if let Some(_) = msg.downcast_ref::<QueryProgramsMessage>() {
            Box::<Option<Result<Vec<EpgProgram>, Error>>>::new(Some(Ok(Vec::new())))
        } else if let Some(msg) = msg.downcast_ref::<QueryProgramMessage>() {
            let result = match msg {
                QueryProgramMessage::ByNidSidEid { nid, sid, eid } => {
                    if eid.value() == 0 {
                        Err(Error::ProgramNotFound)
                    } else {
                        Ok(EpgProgram::new((*nid, 0.into(), *sid, *eid).into()))
                    }
                }
            };
            Box::<Option<Result<EpgProgram, Error>>>::new(Some(result))
        } else if let Some(_) = msg.downcast_ref::<RemoveAirtimeMessage>() {
            Box::<Option<()>>::new(Some(()))
        } else {
            unimplemented!();
        }
    }))
    .start()
}

fn timeshift_manager_for_test() -> Addr<TimeshiftManager> {
    TimeshiftManager::mock(Box::new(|msg, _ctx| {
        if let Some(_) = msg.downcast_ref::<QueryTimeshiftRecordersMessage>() {
            Box::<Option<Result<Vec<TimeshiftRecorderModel>, Error>>>::new(Some(Ok(Vec::new())))
        } else if let Some(msg) = msg.downcast_ref::<QueryTimeshiftRecorderMessage>() {
            let result = match msg.recorder {
                TimeshiftRecorderQuery::ByName(ref name) if name == "test" => {
                    Ok(TimeshiftRecorderModel {
                        index: 0,
                        name: name.clone(),
                        service: EpgService {
                            nid: 1.into(),
                            tsid: 2.into(),
                            sid: 3.into(),
                            service_type: 1,
                            logo_id: 0,
                            remote_control_key_id: 0,
                            name: "test".to_string(),
                            channel: EpgChannel {
                                name: "test".to_string(),
                                channel_type: ChannelType::GR,
                                channel: "test".to_string(),
                                extra_args: "".to_string(),
                                services: Vec::new(),
                                excluded_services: Vec::new(),
                            },
                        },
                        start_time: Jst::now(),
                        end_time: Jst::now(),
                        pipeline: vec![],
                        recording: true,
                    })
                }
                _ => Err(Error::RecordNotFound),
            };
            Box::<Option<Result<_, Error>>>::new(Some(result))
        } else if let Some(_) = msg.downcast_ref::<QueryTimeshiftRecordsMessage>() {
            Box::<Option<Result<Vec<TimeshiftRecordModel>, Error>>>::new(Some(Ok(Vec::new())))
        } else if let Some(msg) = msg.downcast_ref::<QueryTimeshiftRecordMessage>() {
            let result = if msg.record_id == 1u32.into() {
                Ok(TimeshiftRecordModel {
                    id: msg.record_id,
                    program: EpgProgram::new((0, 0, 0, 0).into()),
                    start_time: Jst::now(),
                    end_time: Jst::now(),
                    size: 0,
                    recording: true,
                })
            } else {
                Err(Error::RecordNotFound)
            };
            Box::<Option<Result<_, Error>>>::new(Some(result))
        } else if let Some(msg) = msg.downcast_ref::<CreateTimeshiftLiveStreamSourceMessage>() {
            let result = match msg.recorder {
                TimeshiftRecorderQuery::ByName(ref name) if name == "test" => {
                    Ok(TimeshiftLiveStreamSource::new_for_test(name))
                }
                _ => Err(Error::NoContent),
            };
            Box::<Option<Result<_, Error>>>::new(Some(result))
        } else if let Some(msg) = msg.downcast_ref::<CreateTimeshiftRecordStreamSourceMessage>() {
            let result = match msg.recorder {
                TimeshiftRecorderQuery::ByName(ref name) if name == "test" => {
                    Ok(TimeshiftRecordStreamSource::new_for_test(name))
                }
                _ => Err(Error::NoContent),
            };
            Box::<Option<Result<_, Error>>>::new(Some(result))
        } else {
            unimplemented!();
        }
    }))
    .start()
}

fn user_for_test(priority: TunerUserPriority) -> TunerUser {
    TunerUser {
        info: TunerUserInfo::Web {
            id: "".to_string(),
            agent: None,
        },
        priority,
    }
}

mod helper {
    use std::net::SocketAddr;
    use std::task::Context;
    use std::task::Poll;

    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::Request;
    use axum::response::Response;
    use futures::future::BoxFuture;
    use tower::Layer;
    use tower::Service;

    pub(super) struct ReplaceConnectInfoLayer(Option<SocketAddr>);

    impl ReplaceConnectInfoLayer {
        pub(super) fn new(addr: Option<SocketAddr>) -> Self {
            ReplaceConnectInfoLayer(addr)
        }
    }

    impl<S> Layer<S> for ReplaceConnectInfoLayer {
        type Service = ReplaceConnectInfoService<S>;

        fn layer(&self, inner: S) -> Self::Service {
            ReplaceConnectInfoService {
                inner,
                addr: self.0.clone(),
            }
        }
    }

    #[derive(Clone)]
    pub(super) struct ReplaceConnectInfoService<S> {
        inner: S,
        addr: Option<SocketAddr>,
    }

    impl<S> Service<Request<Body>> for ReplaceConnectInfoService<S>
    where
        S: Service<Request<Body>, Response = Response> + Send + 'static,
        S::Future: Send + 'static,
    {
        type Response = S::Response;
        type Error = S::Error;
        // `BoxFuture` is a type alias for `Pin<Box<dyn Future + Send + 'a>>`
        type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.inner.poll_ready(cx)
        }

        fn call(&mut self, mut req: Request<Body>) -> Self::Future {
            match self.addr {
                Some(addr) => {
                    req.extensions_mut().insert(ConnectInfo(addr.clone()));
                }
                None => {
                    req.extensions_mut().remove::<ConnectInfo<SocketAddr>>();
                }
            }
            let fut = self.inner.call(req);
            Box::pin(async move { Ok(fut.await?) })
        }
    }
}