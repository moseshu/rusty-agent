// Adapted from openai/codex (Apache-2.0) @ e58bf8f5233b51e7cd1679d947e3028c1ed91b06, modified.
//! HTTP helper credential precedence and replay through the normal MCP transport.
use super::*;

#[tokio::test]
async fn helper_refreshes_changed_headers_once_and_keeps_explicit_authorization() {
    for routed in [false, true] {
        let fixture = Server::new().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let mut params = fixture.params().with_headers(
            [
                ("x-helper".to_owned(), "static".to_owned()),
                ("Authorization".into(), "Bearer explicit".into()),
            ]
            .into(),
        );
        if routed {
            params = params.with_route_factory(ra_http::HttpClientFactory::new(
                ra_http::OutboundProxyPolicy::ReqwestDefault,
            ));
        }
        let params = params.with_http_headers_helper({
            let calls = calls.clone();
            let state = fixture.state.clone();
            move |_, _| {
                let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if attempt == 2 {
                    state.behavior.lock().unwrap().request_status = None;
                }
                async move {
                    Ok(json!({"authorization": "Bearer helper", "x-helper": format!("version-{attempt}")})
                        .to_string().into_bytes())
                }
            }
        });

        let client = McpClient::connect_http("helper", params, McpClientOptions::default())
            .await
            .unwrap();
        fixture.state.behavior.lock().unwrap().request_status = Some(StatusCode::UNAUTHORIZED);
        client.list_tools().await.unwrap();
        client.list_tools().await.unwrap();
        client.cleanup().await.unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let seen = fixture.requests("tools/list");
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].body, seen[1].body);
        assert_eq!(seen[0].headers["x-helper"], "version-1");
        assert_eq!(seen[1].headers["x-helper"], "version-2");
        assert_eq!(seen[2].headers["x-helper"], "version-2");
        assert!(
            seen.iter()
                .all(|request| request.headers["authorization"] == "Bearer explicit")
        );
        assert_eq!(
            fixture.requests("DELETE")[0].headers["x-helper"],
            "version-2"
        );
    }
}

#[derive(Clone, Copy)]
enum RefreshOutcome {
    Unchanged,
    Failed,
    Changed,
}

#[tokio::test]
async fn helper_keeps_original_auth_rejections_and_limits_replay() {
    use RefreshOutcome::{Changed, Failed, Unchanged};
    for (outcome, status, challenge, calls_expected, requests_expected) in [
        (Unchanged, StatusCode::UNAUTHORIZED, None, 2, 1),
        (Failed, StatusCode::UNAUTHORIZED, None, 2, 1),
        (Changed, StatusCode::UNAUTHORIZED, None, 2, 2),
        (Changed, StatusCode::FORBIDDEN, None, 2, 2),
        (
            Changed,
            StatusCode::FORBIDDEN,
            Some(r#"Basic realm="other", Bearer realm="tools", error="insufficient_scope""#),
            1,
            1,
        ),
    ] {
        let fixture = Server::new().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let params = fixture.params().with_http_headers_helper({
            let calls = calls.clone();
            move |_, _| {
                let attempt = calls.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    match (outcome, attempt) {
                        (Failed, 2) => Err(ra_core::error::Error::config("private-helper-error")),
                        (Changed, _) => Ok(json!({"x-credential": format!("private-{attempt}")})
                            .to_string()
                            .into_bytes()),
                        _ => Ok(br#"{"x-credential":"private-1"}"#.to_vec()),
                    }
                }
            }
        });
        let client = McpClient::connect_http("helper", params, McpClientOptions::default())
            .await
            .unwrap();
        {
            let mut behavior = fixture.state.behavior.lock().unwrap();
            behavior.request_status = Some(status);
            behavior.request_challenge = challenge;
        }
        let error = client.list_tools().await.unwrap_err();
        assert!(!format!("{error:?}").contains("private"));
        assert_eq!(calls.load(Ordering::SeqCst), calls_expected);
        assert_eq!(fixture.requests("tools/list").len(), requests_expected);
        client.cleanup().await.unwrap();
    }
}

#[tokio::test]
async fn helper_credentials_stay_on_the_original_origin() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method},
    };

    let other = Server::new().await;
    let primary = MockServer::start().await;
    Mock::given(method("POST"))
        .and(header("proxy-authorization", "Bearer helper-secret"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", other.url.as_str()))
        .expect(1)
        .mount(&primary)
        .await;
    let redirected = McpHttpParams::new(primary.uri()).with_http_headers_helper(|_, _| async {
        Ok(br#"{"proxy-authorization":"Bearer helper-secret"}"#.to_vec())
    });
    let error = McpClient::connect_http("helper", redirected, McpClientOptions::default())
        .await
        .err()
        .expect("redirect must not be followed");
    assert!(!format!("{error:?}").contains("helper-secret"));
    primary.verify().await;
    assert!(other.requests("initialize").is_empty());

    let params = McpHttpParams::new(primary.uri())
        .with_http_headers_helper(|_, _| async {
            panic!("a cross-origin request must not run the helper")
        })
        .with_auth({
            let url = other.url.parse::<reqwest_for_test::Url>().unwrap();
            move |request| {
                let url = url.clone();
                async move {
                    let (client, request) = request.build_split();
                    let mut request = request.unwrap();
                    *request.url_mut() = url;
                    Ok(reqwest_for_test::RequestBuilder::from_parts(
                        client, request,
                    ))
                }
            }
        });
    let client = McpClient::connect_http("helper", params, McpClientOptions::default())
        .await
        .unwrap();
    client.cleanup().await.unwrap();
    assert!(
        !other.requests("initialize")[0]
            .headers
            .contains_key("proxy-authorization")
    );
}

use ra_mcp::client::http::reqwest as reqwest_for_test;
use ra_mcp::client::http::transport_for_test;
use rmcp::{model::ClientJsonRpcMessage, transport::streamable_http_client::StreamableHttpClient};

fn list_message(id: u32) -> ClientJsonRpcMessage {
    serde_json::from_value(json!({"jsonrpc":"2.0", "id":id, "method":"tools/list"})).unwrap()
}

#[tokio::test]
async fn malformed_helper_output_is_rejected_before_sending_any_request() {
    for output in [
        br#"{"X-Secret":"private","X-Secret":"private"}"#.as_slice(),
        br#"{"X-Secret":"private","x-secret":"private"}"#,
        br#"{"Host":"private"}"#,
        br#"{"X-Secret":"private\r\n"}"#,
        br#"{"X-Secret":null}"#,
        b"private, not JSON",
    ] {
        let fixture = Server::new().await;
        let params = fixture
            .params()
            .with_http_headers_helper(move |_, _| async move { Ok(output.to_vec()) });
        let error = McpClient::connect_http("helper", params, McpClientOptions::default())
            .await
            .err()
            .expect("invalid output must fail initialization");
        assert!(!format!("{error:?}").contains("private"));
        assert!(fixture.requests("initialize").is_empty());
    }
}

#[tokio::test]
async fn concurrent_rejections_share_one_helper_refresh() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method},
    };

    let server = MockServer::start().await;
    let rejected = Arc::new(AtomicUsize::new(0));
    let both_rejected = Arc::new(Notify::new());
    Mock::given(method("POST"))
        .and(header("x-helper", "old"))
        .respond_with({
            let rejected = rejected.clone();
            let both_rejected = both_rejected.clone();
            move |_: &wiremock::Request| {
                if rejected.fetch_add(1, Ordering::SeqCst) == 1 {
                    both_rejected.notify_one();
                }
                ResponseTemplate::new(401)
            }
        })
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(header("x-helper", "new"))
        .respond_with(ResponseTemplate::new(204))
        .expect(2)
        .mount(&server)
        .await;

    let calls = Arc::new(AtomicUsize::new(0));
    let params = McpHttpParams::new(server.uri()).with_http_headers_helper({
        let calls = calls.clone();
        move |_, _| {
            let attempt = calls.fetch_add(1, Ordering::SeqCst);
            let both_rejected = both_rejected.clone();
            async move {
                let value = if attempt == 0 {
                    "old"
                } else {
                    tokio::time::timeout(Duration::from_secs(3), both_rejected.notified())
                        .await
                        .unwrap();
                    "new"
                };
                Ok(json!({"x-helper":value}).to_string().into_bytes())
            }
        }
    });
    let transport = transport_for_test(&params).unwrap();
    let uri: Arc<str> = Arc::from(server.uri());
    let post = |id| {
        transport.post_message(
            uri.clone(),
            list_message(id),
            None,
            None,
            Default::default(),
        )
    };
    let (first, second) = tokio::join!(post(1), post(2));
    assert!(first.is_ok(), "{first:?}");
    assert!(second.is_ok(), "{second:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.verify().await;
}

#[tokio::test(start_paused = true)]
async fn hanging_helper_is_bounded_by_its_own_timeout() {
    let uri: Arc<str> = Arc::from("http://127.0.0.1:1/mcp");
    let params =
        McpHttpParams::new(uri.as_ref()).with_http_headers_helper(|_, _| std::future::pending());
    let transport = transport_for_test(&params).unwrap();
    let started = tokio::time::Instant::now();
    let error = transport
        .post_message(uri, list_message(1), None, None, Default::default())
        .await
        .unwrap_err();
    assert_eq!(started.elapsed(), Duration::from_secs(10));
    assert!(error.to_string().contains("helper timed out"), "{error}");
}

#[tokio::test]
async fn helper_attempt_is_shared_after_cancellation() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method},
    };

    for cancel_refresh in [false, true] {
        let server = MockServer::start().await;
        if cancel_refresh {
            Mock::given(method("POST"))
                .and(header("x-helper", "old"))
                .respond_with(ResponseTemplate::new(401))
                .expect(2)
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .and(header("x-helper", "new"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let calls = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let finished = Arc::new(Notify::new());
        let params = McpHttpParams::new(server.uri()).with_http_headers_helper({
            let (calls, started, release, finished) = (
                calls.clone(),
                started.clone(),
                release.clone(),
                finished.clone(),
            );
            move |_, _| {
                let attempt = calls.fetch_add(1, Ordering::SeqCst);
                let (started, release, finished) =
                    (started.clone(), release.clone(), finished.clone());
                async move {
                    if cancel_refresh && attempt == 0 {
                        return Ok(br#"{"x-helper":"old"}"#.to_vec());
                    }
                    started.notify_one();
                    release.notified().await;
                    finished.notify_one();
                    Ok(br#"{"x-helper":"new"}"#.to_vec())
                }
            }
        });
        let transport = transport_for_test(&params).unwrap();
        let uri: Arc<str> = Arc::from(server.uri());
        {
            let request = transport.post_message(
                uri.clone(),
                list_message(1),
                None,
                None,
                Default::default(),
            );
            tokio::select! {
                result = request => panic!("helper should still be running: {result:?}"),
                result = tokio::time::timeout(Duration::from_secs(3), started.notified()) => result.unwrap(),
            }
        }
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(3), finished.notified())
            .await
            .unwrap();
        transport
            .post_message(uri, list_message(2), None, None, Default::default())
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            if cancel_refresh { 2 } else { 1 }
        );
        server.verify().await;
    }
}

#[tokio::test]
async fn nonzero_helper_exit_is_cached() {
    use ra_mcp::client::http::HttpHeadersHelperProcessError;

    let fixture = Server::new().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let params = fixture.params().with_http_headers_helper({
        let calls = calls.clone();
        move |_, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            async {
                #[cfg(windows)]
                let status = tokio::process::Command::new("cmd.exe")
                    .args(["/D", "/C", "exit /b 23"])
                    .status()
                    .await
                    .unwrap();
                #[cfg(not(windows))]
                let status = tokio::process::Command::new("sh")
                    .args(["-c", "exit 23"])
                    .status()
                    .await
                    .unwrap();
                Err(ra_core::error::Error::config("private-helper-error")
                    .with_source(HttpHeadersHelperProcessError::Exit(status)))
            }
        }
    });
    let transport = transport_for_test(&params).unwrap();
    let uri: Arc<str> = Arc::from(fixture.url.as_str());
    for id in [1, 2] {
        let error = transport
            .post_message(
                uri.clone(),
                list_message(id),
                None,
                None,
                Default::default(),
            )
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("exited with status 23"),
            "{error}"
        );
        assert!(!format!("{error:?}").contains("private-helper-error"));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(fixture.requests("tools/list").is_empty());
}

#[tokio::test]
async fn refresh_retry_rechecks_deadline_and_redirects() {
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, method},
    };

    for redirect in [false, true] {
        let server = MockServer::start().await;
        let other = Server::new().await;
        let rejected = ResponseTemplate::new(401);
        let (rejected, retried, expected) = if redirect {
            (
                rejected,
                ResponseTemplate::new(307).insert_header("location", other.url.as_str()),
                "cannot safely replay Proxy-Authorization",
            )
        } else {
            (
                rejected.set_delay(Duration::from_millis(500)),
                ResponseTemplate::new(204).set_delay(Duration::from_millis(1250)),
                "request timed out",
            )
        };
        Mock::given(method("POST"))
            .and(header("x-helper", "old"))
            .respond_with(rejected)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(header("proxy-authorization", "Bearer fresh"))
            .respond_with(retried)
            .expect(1)
            .mount(&server)
            .await;

        let calls = Arc::new(AtomicUsize::new(0));
        let params = McpHttpParams::new(server.uri())
            .with_auth(|request| async move { Ok(request.timeout(Duration::from_millis(1500))) })
            .with_http_headers_helper({
                let calls = calls.clone();
                move |_, _| {
                    let attempt = calls.fetch_add(1, Ordering::SeqCst);
                    async move {
                        Ok(if attempt == 0 {
                            br#"{"x-helper":"old"}"#.to_vec()
                        } else {
                            br#"{"proxy-authorization":"Bearer fresh"}"#.to_vec()
                        })
                    }
                }
            });
        let transport = transport_for_test(&params).unwrap();
        let error = transport
            .post_message(
                Arc::from(server.uri()),
                list_message(1),
                None,
                None,
                Default::default(),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(other.requests("POST").is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.verify().await;
    }
}
