//! HTTP execution tests against an in-process `wiremock` server (no real network).

use std::sync::Arc;
use std::time::Duration;

use churl_core::config::RedirectPolicy;
use churl_core::cookies::ChurlCookieJar;
use churl_core::http::{
    ClientConfig, DEFAULT_TIMEOUT, ExecuteOptions, HttpError, build_client, build_client_with,
    execute, follow_all_warned,
};
use churl_core::model::{ApiKeyPlacement, Auth, Body, BodyKind, Header, Method, Param, Request};
use wiremock::matchers::{body_string, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Builds a bare GET request to `url`.
fn get(url: String) -> Request {
    Request {
        method: Method::Get,
        url,
        headers: Vec::new(),
        params: Vec::new(),
        body: None,
        auth: None,
    }
}

#[tokio::test]
async fn get_200_returns_status_headers_and_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/users"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-total", "7")
                .set_body_string("hello world"),
        )
        .mount(&server)
        .await;

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(
        &client,
        &get(format!("{}/users", server.uri())),
        &ExecuteOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"hello world");
    assert!(
        response
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("x-total") && h.value == "7"),
        "expected x-total header, got {:?}",
        response.headers
    );
    assert!(response.timing.connect.is_none());
}

#[tokio::test]
async fn post_body_derives_content_type() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/items"))
        .and(header("content-type", "application/json"))
        .and(body_string(r#"{"a":1}"#))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/items", server.uri()));
    request.method = Method::Post;
    request.body = Some(Body {
        kind: BodyKind::Json,
        content: r#"{"a":1}"#.to_owned(),
    });

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 201);
}

#[tokio::test]
async fn user_content_type_header_overrides_derived() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/items"))
        .and(header("content-type", "application/vnd.custom+json"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/items", server.uri()));
    request.method = Method::Post;
    request.headers = vec![Header {
        name: "Content-Type".to_owned(),
        value: "application/vnd.custom+json".to_owned(),
        enabled: true,
    }];
    request.body = Some(Body {
        kind: BodyKind::Json,
        content: "{}".to_owned(),
    });

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn disabled_header_and_param_are_excluded() {
    // A strict mock that only matches when BOTH the header and the query param are
    // present. If a disabled header/param leaked onto the wire it would match and
    // return 222; excluded correctly, wiremock finds no match and returns 404.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/f"))
        .and(header("x-secret", "leak"))
        .and(query_param("debug", "1"))
        .respond_with(ResponseTemplate::new(222))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/f", server.uri()));
    request.headers = vec![Header {
        name: "X-Secret".to_owned(),
        value: "leak".to_owned(),
        enabled: false,
    }];
    request.params = vec![Param {
        name: "debug".to_owned(),
        value: "1".to_owned(),
        enabled: false,
    }];

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(
        response.status, 404,
        "disabled header/param must not be sent"
    );
}

#[tokio::test]
async fn enabled_param_appends_to_existing_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "rust"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/search?q=rust", server.uri()));
    request.params = vec![Param {
        name: "page".to_owned(),
        value: "2".to_owned(),
        enabled: true,
    }];

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn basic_auth_sends_base64_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/private"))
        // base64("alice:s3cr3t")
        .and(header("authorization", "Basic YWxpY2U6czNjcjN0"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/private", server.uri()));
    request.auth = Some(Auth::Basic {
        username: "alice".to_owned(),
        password: "s3cr3t".to_owned(),
    });

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn bearer_auth_sends_authorization_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header("authorization", "Bearer {{token}}"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/me", server.uri()));
    request.auth = Some(Auth::Bearer {
        token: "{{token}}".to_owned(),
    });

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn apikey_header_auth_sends_named_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/things"))
        .and(header("x-api-key", "{{api_key}}"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/things", server.uri()));
    request.auth = Some(Auth::ApiKey {
        name: "X-Api-Key".to_owned(),
        value: "{{api_key}}".to_owned(),
        placement: ApiKeyPlacement::Header,
    });

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn apikey_query_auth_appends_pair_preserving_existing_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "rust"))
        .and(query_param("page", "2"))
        .and(query_param("api_key", "{{api_key}}"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    // Existing URL query + an enabled param + the auth pair, all on the wire.
    let mut request = get(format!("{}/search?q=rust", server.uri()));
    request.params = vec![Param {
        name: "page".to_owned(),
        value: "2".to_owned(),
        enabled: true,
    }];
    request.auth = Some(Auth::ApiKey {
        name: "api_key".to_owned(),
        value: "{{api_key}}".to_owned(),
        placement: ApiKeyPlacement::Query,
    });

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn enabled_user_authorization_header_beats_auth() {
    // Only the auth-injected value has a mock; if the user's enabled header wins
    // (auth header NOT injected), nothing matches and wiremock returns 404.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/x"))
        .and(header("authorization", "Bearer {{token}}"))
        .respond_with(ResponseTemplate::new(222))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/x", server.uri()));
    request.headers = vec![Header {
        name: "Authorization".to_owned(),
        value: "custom-scheme abc".to_owned(),
        enabled: true,
    }];
    request.auth = Some(Auth::Bearer {
        token: "{{token}}".to_owned(),
    });

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 404, "user's enabled header must win");
}

#[tokio::test]
async fn param_with_space_and_ampersand_is_encoded_correctly_on_wire() {
    // A param value containing a space and `&` must be percent-encoded so the
    // wire URL is not ambiguous. wiremock's query_param matcher decodes before
    // comparing, so a correct match proves the encoding was valid.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search"))
        .and(query_param("q", "hello world&more"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/search", server.uri()));
    request.params = vec![Param {
        name: "q".to_owned(),
        value: "hello world&more".to_owned(),
        enabled: true,
    }];

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200, "space and & in param must be encoded");
}

#[tokio::test]
async fn disabled_user_authorization_header_does_not_beat_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/x"))
        .and(header("authorization", "Bearer {{token}}"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/x", server.uri()));
    request.headers = vec![Header {
        name: "Authorization".to_owned(),
        value: "custom-scheme abc".to_owned(),
        enabled: false,
    }];
    request.auth = Some(Auth::Bearer {
        token: "{{token}}".to_owned(),
    });

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200, "disabled header must not block auth");
}

#[tokio::test]
async fn apikey_header_beaten_by_same_name_enabled_user_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/x"))
        .and(header("x-api-key", "{{api_key}}"))
        .respond_with(ResponseTemplate::new(222))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/x", server.uri()));
    request.headers = vec![Header {
        name: "x-api-key".to_owned(), // case-insensitive match against auth's name
        value: "user-supplied".to_owned(),
        enabled: true,
    }];
    request.auth = Some(Auth::ApiKey {
        name: "X-Api-Key".to_owned(),
        value: "{{api_key}}".to_owned(),
        placement: ApiKeyPlacement::Header,
    });

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 404, "user's same-name header must win");
}

#[tokio::test]
async fn connection_refused_is_an_error() {
    // Port 1 is not listening; connect fails fast.
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let err = execute(
        &client,
        &get("http://127.0.0.1:1/".to_owned()),
        &ExecuteOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, HttpError::Request(_)), "got {err:?}");
}

#[tokio::test]
async fn invalid_url_is_reported() {
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let err = execute(
        &client,
        &get("not a url".to_owned()),
        &ExecuteOptions::default(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, HttpError::InvalidUrl { .. }), "got {err:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborting_the_task_cancels_the_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/slow"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let request = get(format!("{}/slow", server.uri()));
    let handle =
        tokio::spawn(async move { execute(&client, &request, &ExecuteOptions::default()).await });
    // Give the request time to actually go in flight before cancelling it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    handle.abort();
    let joined = handle.await;
    assert!(joined.is_err());
    assert!(joined.unwrap_err().is_cancelled());
}

#[tokio::test]
async fn body_over_cap_is_truncated_at_cap_boundary() {
    let server = MockServer::start().await;
    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    Mock::given(method("GET"))
        .and(path("/big"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(payload.clone()))
        .mount(&server)
        .await;

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let options = ExecuteOptions {
        max_body_bytes: 1024,
        ..ExecuteOptions::default()
    };
    let response = execute(&client, &get(format!("{}/big", server.uri())), &options)
        .await
        .unwrap();

    assert!(response.truncated);
    assert_eq!(response.body.len(), 1024);
    assert_eq!(response.body, payload[..1024]);
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn body_exactly_at_cap_is_not_truncated() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/exact"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'x'; 1024]))
        .mount(&server)
        .await;

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let options = ExecuteOptions {
        max_body_bytes: 1024,
        ..ExecuteOptions::default()
    };
    let response = execute(&client, &get(format!("{}/exact", server.uri())), &options)
        .await
        .unwrap();

    assert!(!response.truncated);
    assert_eq!(response.body.len(), 1024);
}

#[tokio::test]
async fn small_body_under_default_cap_is_unchanged() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/small"))
        .respond_with(ResponseTemplate::new(200).set_body_string("tiny"))
        .mount(&server)
        .await;

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(
        &client,
        &get(format!("{}/small", server.uri())),
        &ExecuteOptions::default(),
    )
    .await
    .unwrap();

    assert!(!response.truncated);
    assert_eq!(response.body, b"tiny");
}

#[tokio::test]
async fn default_user_agent_is_sent() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ua"))
        .and(header(
            "user-agent",
            concat!("churl/", env!("CARGO_PKG_VERSION")),
        ))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(
        &client,
        &get(format!("{}/ua", server.uri())),
        &ExecuteOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(response.status, 200, "default User-Agent header not sent");
}

#[tokio::test]
async fn enabled_user_agent_header_overrides_default() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ua"))
        .and(header("user-agent", "custom-agent/9"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let mut request = get(format!("{}/ua", server.uri()));
    request.headers.push(Header {
        name: "User-Agent".to_owned(),
        value: "custom-agent/9".to_owned(),
        enabled: true,
    });
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200, "user User-Agent header did not win");
}

/// The secret gate is save-time only: an endpoint carrying a *literal* secret
/// (e.g. a bearer token or an `?api_key=` in the URL) still sends unchanged. The
/// send path is never gated — only churl-initiated writes are.
#[tokio::test]
async fn literal_secret_still_sends_send_path_ungated() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/me"))
        .and(header(
            "authorization",
            "Bearer sk-live-literal-token-value",
        ))
        .and(query_param("api_key", "AKIAIOSFODNN7EXAMPLE"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&server)
        .await;

    let mut request = get(format!("{}/me", server.uri()));
    request.auth = Some(Auth::Bearer {
        token: "sk-live-literal-token-value".into(),
    });
    request.params.push(Param {
        name: "api_key".into(),
        value: "AKIAIOSFODNN7EXAMPLE".into(),
        enabled: true,
    });

    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200, "a literal secret must still send");
}

// --- Cross-origin redirect policy (R3 PR-B) ---------------------------------
//
// Two independent `MockServer`s bind distinct ports on 127.0.0.1, so A and B
// differ in *port* — a genuine cross-origin boundary under the scheme+host+port
// origin definition. B's mock always answers 200, and we inspect
// `b.received_requests()` to assert exactly which headers actually reached B.

/// Builds a GET to `url` carrying an `Authorization`, a `Cookie`, and a
/// secret-named `X-Api-Key` header — the three auth-bearing headers the strip
/// policy must drop on a cross-origin hop.
fn get_with_auth_headers(url: String) -> Request {
    let mut request = get(url);
    request.headers = vec![
        Header {
            name: "Authorization".to_owned(),
            value: "Bearer secret-token".to_owned(),
            enabled: true,
        },
        Header {
            name: "Cookie".to_owned(),
            value: "session=abc".to_owned(),
            enabled: true,
        },
        Header {
            name: "X-Api-Key".to_owned(),
            value: "sk-live-123".to_owned(),
            enabled: true,
        },
    ];
    request
}

/// Returns the single request B received (panics if none), for header assertions.
async fn only_request(server: &MockServer) -> wiremock::Request {
    let mut reqs = server
        .received_requests()
        .await
        .expect("request recording is enabled");
    assert_eq!(reqs.len(), 1, "B should have been hit exactly once");
    reqs.pop().unwrap()
}

fn has_header(req: &wiremock::Request, name: &str) -> bool {
    req.headers.contains_key(name)
}

#[tokio::test]
async fn strip_default_drops_auth_headers_cross_origin() {
    let origin_a = MockServer::start().await;
    let origin_b = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/dest"))
        .respond_with(ResponseTemplate::new(200).set_body_string("landed"))
        .mount(&origin_b)
        .await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/dest", origin_b.uri())),
        )
        .mount(&origin_a)
        .await;

    let request = get_with_auth_headers(format!("{}/start", origin_a.uri()));
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    // Default options resolve to Strip.
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();

    assert_eq!(
        response.status, 200,
        "the cross-origin redirect is followed"
    );
    assert_eq!(response.body, b"landed");

    let at_b = only_request(&origin_b).await;
    assert!(
        !has_header(&at_b, "authorization"),
        "Authorization must be stripped cross-origin"
    );
    assert!(
        !has_header(&at_b, "cookie"),
        "Cookie must be stripped cross-origin"
    );
    assert!(
        !has_header(&at_b, "x-api-key"),
        "a secret-named header must be stripped cross-origin"
    );
}

#[tokio::test]
async fn strip_preserves_auth_headers_same_origin() {
    let origin_a = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/dest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&origin_a)
        .await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/dest"))
        .mount(&origin_a)
        .await;

    let request = get_with_auth_headers(format!("{}/start", origin_a.uri()));
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);

    // Two requests hit A (/start then /dest); the followed one keeps all headers.
    let reqs = origin_a.received_requests().await.unwrap();
    let dest = reqs
        .iter()
        .find(|r| r.url.path() == "/dest")
        .expect("the same-origin hop was followed to /dest");
    assert!(
        has_header(dest, "authorization")
            && has_header(dest, "cookie")
            && has_header(dest, "x-api-key"),
        "same-origin redirects keep all headers"
    );
}

#[tokio::test]
async fn strict_stops_and_surfaces_cross_origin_redirect() {
    let origin_a = MockServer::start().await;
    let origin_b = MockServer::start().await;

    // If churl (wrongly) follows to B, this records a hit we assert against.
    Mock::given(method("GET"))
        .and(path("/dest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&origin_b)
        .await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/dest", origin_b.uri())),
        )
        .mount(&origin_a)
        .await;

    let request = get(format!("{}/start", origin_a.uri()));
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let options = ExecuteOptions {
        redirect: RedirectPolicy::Strict,
        ..ExecuteOptions::default()
    };
    let response = execute(&client, &request, &options).await.unwrap();

    assert_eq!(
        response.status, 302,
        "strict surfaces the cross-origin 3xx instead of following it"
    );
    assert!(
        response
            .headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("location")),
        "the Location header is surfaced to the user"
    );
    assert!(
        origin_b.received_requests().await.unwrap().is_empty(),
        "strict must not hit the cross-origin target"
    );
}

#[tokio::test]
async fn strict_follows_same_origin_redirect() {
    let origin_a = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/dest"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&origin_a)
        .await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/dest"))
        .mount(&origin_a)
        .await;

    let request = get(format!("{}/start", origin_a.uri()));
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let options = ExecuteOptions {
        redirect: RedirectPolicy::Strict,
        ..ExecuteOptions::default()
    };
    let response = execute(&client, &request, &options).await.unwrap();
    assert_eq!(
        response.status, 200,
        "strict follows a same-origin redirect"
    );
    assert_eq!(response.body, b"ok");
}

#[tokio::test]
async fn follow_all_keeps_auth_headers_cross_origin_and_warns_once() {
    let origin_a = MockServer::start().await;
    let origin_b = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/dest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&origin_b)
        .await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/dest", origin_b.uri())),
        )
        .mount(&origin_a)
        .await;

    let request = get_with_auth_headers(format!("{}/start", origin_a.uri()));
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let options = ExecuteOptions {
        redirect: RedirectPolicy::FollowAll,
        ..ExecuteOptions::default()
    };
    let response = execute(&client, &request, &options).await.unwrap();
    assert_eq!(response.status, 200);

    let at_b = only_request(&origin_b).await;
    assert!(
        has_header(&at_b, "authorization")
            && has_header(&at_b, "cookie")
            && has_header(&at_b, "x-api-key"),
        "follow-all keeps all headers across origins (the foot-gun)"
    );
    assert!(
        follow_all_warned(),
        "the one-time follow-all warning must have fired"
    );
}

#[tokio::test]
async fn strip_treats_port_change_as_cross_origin() {
    // Same host (127.0.0.1), different port — a cross-origin hop under the
    // scheme+host+port origin definition, so auth headers must be stripped.
    let origin_a = MockServer::start().await;
    let origin_b = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/dest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&origin_b)
        .await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/dest", origin_b.uri())),
        )
        .mount(&origin_a)
        .await;

    // Sanity: A and B share a host and differ only by port.
    let host_a = origin_a.uri();
    let host_b = origin_b.uri();
    assert!(
        host_a.contains("127.0.0.1") && host_b.contains("127.0.0.1") && host_a != host_b,
        "A and B differ only by port: {host_a} vs {host_b}"
    );

    let request = get_with_auth_headers(format!("{}/start", origin_a.uri()));
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);

    let at_b = only_request(&origin_b).await;
    assert!(
        !has_header(&at_b, "authorization") && !has_header(&at_b, "x-api-key"),
        "a port change alone crosses the origin and strips auth headers"
    );
}

#[tokio::test]
async fn strip_same_origin_then_cross_origin_still_strips_on_the_cross_hop() {
    // The subtle chain: A (auth) -> same-origin /hop (keeps auth) -> B
    // (cross-origin). The cross-origin hop must still strip, even though the
    // header check is per-hop against the immediate predecessor. Once dropped
    // the credentials can never reappear.
    let origin_a = MockServer::start().await;
    let origin_b = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/dest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&origin_b)
        .await;
    // A same-origin hop that KEEPS all headers, then bounces cross-origin to B.
    Mock::given(method("GET"))
        .and(path("/hop"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/dest", origin_b.uri())),
        )
        .mount(&origin_a)
        .await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/hop"))
        .mount(&origin_a)
        .await;

    let request = get_with_auth_headers(format!("{}/start", origin_a.uri()));
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);

    // The same-origin /hop kept the auth headers...
    let reqs_a = origin_a.received_requests().await.unwrap();
    let hop = reqs_a
        .iter()
        .find(|r| r.url.path() == "/hop")
        .expect("the same-origin hop was followed");
    assert!(
        has_header(hop, "authorization") && has_header(hop, "x-api-key"),
        "the same-origin hop keeps auth headers"
    );
    // ...but B (cross-origin) received none of them.
    let at_b = only_request(&origin_b).await;
    assert!(
        !has_header(&at_b, "authorization")
            && !has_header(&at_b, "cookie")
            && !has_header(&at_b, "x-api-key"),
        "the cross-origin hop after a same-origin hop still strips auth headers"
    );
}

#[tokio::test]
async fn strip_307_preserves_body_but_strips_auth_cross_origin() {
    // 307 preserves method + body across the hop; auth headers must still be
    // stripped when it crosses the origin.
    let origin_a = MockServer::start().await;
    let origin_b = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/dest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&origin_b)
        .await;
    Mock::given(method("POST"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/dest", origin_b.uri())),
        )
        .mount(&origin_a)
        .await;

    let mut request = get_with_auth_headers(format!("{}/start", origin_a.uri()));
    request.method = Method::Post;
    request.body = Some(Body {
        kind: BodyKind::Json,
        content: r#"{"k":"v"}"#.to_owned(),
    });
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);

    let at_b = only_request(&origin_b).await;
    assert_eq!(at_b.method.as_str(), "POST", "307 preserves the method");
    assert_eq!(at_b.body, br#"{"k":"v"}"#, "307 preserves the body");
    assert!(
        !has_header(&at_b, "authorization") && !has_header(&at_b, "x-api-key"),
        "307 still strips auth headers on a cross-origin hop"
    );
}

/// Builds a GET to `url` carrying one custom header `name: value`.
fn get_with_header(url: String, name: &str, value: &str) -> Request {
    let mut request = get(url);
    request.headers = vec![Header {
        name: name.to_owned(),
        value: value.to_owned(),
        enabled: true,
    }];
    request
}

#[tokio::test]
async fn strip_drops_secret_shaped_value_under_innocent_name_cross_origin() {
    // The value anchor: an innocent-NAMED header ("auth" is not a secret-name
    // marker) whose VALUE is a real token must NOT reach a foreign origin.
    // Fails on a name-only strip set; passes with the value anchor.
    let origin_a = MockServer::start().await;
    let origin_b = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/dest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&origin_b)
        .await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/dest", origin_b.uri())),
        )
        .mount(&origin_a)
        .await;

    let request = get_with_header(
        format!("{}/start", origin_a.uri()),
        "X-Custom-Auth",
        "sk-live-LEAKME1234567890abcDEF",
    );
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);

    let at_b = only_request(&origin_b).await;
    assert!(
        !has_header(&at_b, "x-custom-auth"),
        "a secret-shaped value under an opaque name must be stripped cross-origin"
    );
}

#[tokio::test]
async fn strip_keeps_secret_shaped_value_header_same_origin() {
    // The same opaque-named secret-shaped header is preserved on a SAME-origin
    // hop — stripping only bites when the origin actually changes.
    let origin_a = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/dest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&origin_a)
        .await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/dest"))
        .mount(&origin_a)
        .await;

    let request = get_with_header(
        format!("{}/start", origin_a.uri()),
        "X-Custom-Auth",
        "sk-live-LEAKME1234567890abcDEF",
    );
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);

    let reqs = origin_a.received_requests().await.unwrap();
    let dest = reqs
        .iter()
        .find(|r| r.url.path() == "/dest")
        .expect("the same-origin hop was followed");
    assert!(
        has_header(dest, "x-custom-auth"),
        "same-origin hops keep even a secret-shaped header"
    );
}

#[tokio::test]
async fn strip_does_not_over_strip_innocent_header_cross_origin() {
    // The value anchor must not over-strip: an innocent NAME and innocent VALUE
    // (a short low-entropy request id) survives a cross-origin hop.
    let origin_a = MockServer::start().await;
    let origin_b = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/dest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&origin_b)
        .await;
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("location", format!("{}/dest", origin_b.uri())),
        )
        .mount(&origin_a)
        .await;

    let request = get_with_header(
        format!("{}/start", origin_a.uri()),
        "X-Request-Id",
        "abc123",
    );
    let client = build_client(DEFAULT_TIMEOUT).unwrap();
    let response = execute(&client, &request, &ExecuteOptions::default())
        .await
        .unwrap();
    assert_eq!(response.status, 200);

    let at_b = only_request(&origin_b).await;
    assert!(
        has_header(&at_b, "x-request-id"),
        "a plain non-secret header must survive a cross-origin hop"
    );
}

// ---- proxy + cookies (M8) --------------------------------------------

#[test]
fn build_client_with_bad_proxy_fails_loud() {
    // A malformed proxy URL must fail the build, never silently fall back to a
    // direct (unproxied) connection.
    let result = build_client_with(&ClientConfig {
        proxy: Some("::: not a url :::".to_owned()),
        ..Default::default()
    });
    assert!(result.is_err(), "a malformed proxy must fail the build");
}

#[test]
fn build_client_with_valid_proxy_and_no_proxy_both_build() {
    assert!(
        build_client_with(&ClientConfig {
            proxy: Some("http://proxy.local:3128".to_owned()),
            ..Default::default()
        })
        .is_ok()
    );
    // No proxy set → still builds (reqwest then honors the env proxy).
    assert!(build_client_with(&ClientConfig::default()).is_ok());
}

#[tokio::test]
async fn cookie_jar_stores_set_cookie_and_sends_it_same_origin() {
    let server = MockServer::start().await;
    // First hop hands out a Set-Cookie.
    Mock::given(method("GET"))
        .and(path("/login"))
        .respond_with(ResponseTemplate::new(200).insert_header("set-cookie", "sid=abc123; Path=/"))
        .mount(&server)
        .await;
    // Second hop must carry the cookie back.
    Mock::given(method("GET"))
        .and(path("/whoami"))
        .and(header("cookie", "sid=abc123"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let jar = Arc::new(ChurlCookieJar::new());
    let client = build_client_with(&ClientConfig {
        timeout: DEFAULT_TIMEOUT,
        cookies: Some(jar.clone()),
        ..Default::default()
    })
    .unwrap();

    let first = execute(
        &client,
        &get(format!("{}/login", server.uri())),
        &ExecuteOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(first.status, 200);

    let second = execute(
        &client,
        &get(format!("{}/whoami", server.uri())),
        &ExecuteOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(second.status, 200, "the jar must send the stored cookie");
    assert_eq!(second.body, b"ok");
}

#[tokio::test]
async fn cookie_survives_same_origin_redirect() {
    let server = MockServer::start().await;
    // /start sets a cookie AND redirects (same origin) to /land.
    Mock::given(method("GET"))
        .and(path("/start"))
        .respond_with(
            ResponseTemplate::new(302)
                .insert_header("set-cookie", "sid=hop; Path=/")
                .insert_header("location", "/land"),
        )
        .mount(&server)
        .await;
    // /land must receive the cookie set on the previous same-origin hop.
    Mock::given(method("GET"))
        .and(path("/land"))
        .and(header("cookie", "sid=hop"))
        .respond_with(ResponseTemplate::new(200).set_body_string("landed"))
        .mount(&server)
        .await;

    let jar = Arc::new(ChurlCookieJar::new());
    let client = build_client_with(&ClientConfig {
        timeout: DEFAULT_TIMEOUT,
        cookies: Some(jar.clone()),
        ..Default::default()
    })
    .unwrap();

    let response = execute(
        &client,
        &get(format!("{}/start", server.uri())),
        &ExecuteOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(response.status, 200);
    assert_eq!(
        response.body, b"landed",
        "cookie must cross the same-origin hop"
    );
}
