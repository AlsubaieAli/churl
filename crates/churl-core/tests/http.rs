//! HTTP execution tests against an in-process `wiremock` server (no real network).

use std::time::Duration;

use churl_core::http::{DEFAULT_TIMEOUT, ExecuteOptions, HttpError, build_client, execute};
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
