//! Insecure-TLS behaviour against a REAL self-signed TLS server (not a mock):
//! the secure client rejects an untrusted cert, the insecure client accepts both
//! an untrusted cert AND a hostname mismatch. This exercises the rustls
//! `danger_accept_invalid_certs(true)` semantics end-to-end, which is the whole
//! point of the feature and the corner a reviewer will attack.

use std::sync::Arc;
use std::time::Duration;

use churl_core::http::{ClientConfig, build_client, execute};
use churl_core::model::{Method, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

fn get(url: String) -> Request {
    Request {
        method: Method::Get,
        url,
        headers: Vec::new(),
        params: Vec::new(),
        body: None,
        auth: None,
        insecure: false,
    }
}

/// Spawns a minimal self-signed HTTPS server whose cert is valid for `san`
/// (a hostname/SAN). It always LISTENS on 127.0.0.1, so passing a `san` other
/// than `127.0.0.1` produces a genuine hostname mismatch when a client connects
/// to the returned `https://127.0.0.1:PORT/` URL. Returns that URL. The server
/// task loops, serving a tiny `200 ok` to every successful TLS handshake, until
/// the test ends (the task is detached and dropped with the runtime).
async fn spawn_self_signed(san: &str) -> String {
    let cert = rcgen::generate_simple_self_signed(vec![san.to_owned()]).unwrap();
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()));

    // Both `ring` and `aws-lc-rs` are in the dependency tree (reqwest pulls one,
    // this test's tokio-rustls the other), so the process-level default provider
    // is ambiguous — pin the server explicitly to `ring` rather than relying on a
    // global install.
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let server_config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    // Drain the request line(s) enough to respond; we don't parse.
                    let mut buf = [0u8; 1024];
                    let _ = tls.read(&mut buf).await;
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                    let _ = tls.flush().await;
                }
            });
        }
    });

    format!("https://127.0.0.1:{}/", addr.port())
}

#[tokio::test]
async fn secure_client_rejects_self_signed_cert() {
    // Cert valid for 127.0.0.1 (no hostname mismatch), but self-signed and
    // untrusted — the verifying client must refuse it.
    let url = spawn_self_signed("127.0.0.1").await;
    let client = build_client(Duration::from_secs(5)).unwrap();
    let result = execute(&client, &get(url), &Default::default()).await;
    assert!(
        result.is_err(),
        "the secure client must reject a self-signed cert"
    );
}

#[tokio::test]
async fn insecure_client_accepts_self_signed_cert() {
    let url = spawn_self_signed("127.0.0.1").await;
    let client = build_client_insecure();
    let response = execute(&client, &get(url), &Default::default())
        .await
        .expect("the insecure client must accept a self-signed cert");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"ok");
}

#[tokio::test]
async fn insecure_client_accepts_hostname_mismatch() {
    // Cert is valid for "wrong.host" but the client connects to 127.0.0.1 — a
    // genuine hostname mismatch. rustls' danger flag disables hostname
    // verification too, so the insecure client accepts it.
    let url = spawn_self_signed("wrong.host").await;
    let client = build_client_insecure();
    let response = execute(&client, &get(url), &Default::default())
        .await
        .expect("insecure client must accept a hostname-mismatched cert");
    assert_eq!(response.status, 200);
}

#[tokio::test]
async fn secure_client_rejects_hostname_mismatch() {
    let url = spawn_self_signed("wrong.host").await;
    let client = build_client(Duration::from_secs(5)).unwrap();
    assert!(
        execute(&client, &get(url), &Default::default())
            .await
            .is_err(),
        "the secure client must reject a hostname-mismatched cert"
    );
}

#[tokio::test]
async fn per_endpoint_insecure_diverges_within_one_session() {
    // Proves the http-layer primitive the per-endpoint feature relies on: a
    // verifying client and an insecure client — the two `App::client_for` chooses
    // between for a secure session vs. an opted-in endpoint — behave differently
    // against the SAME self-signed server (verify rejects, insecure accepts). This
    // exercises `build_client`/`build_client_with` directly, NOT `App::client_for`
    // (that selection logic is unit-tested in the `churl` binary); it isolates the
    // TLS-verification difference to the client's insecure flag alone.
    let url = spawn_self_signed("127.0.0.1").await;

    // The client a secure session/endpoint uses (verifies).
    let verifying = build_client(Duration::from_secs(5)).unwrap();
    // The client an opted-in endpoint uses: same knobs, verify off.
    let opted_in = build_client_insecure();

    assert!(
        execute(&verifying, &get(url.clone()), &Default::default())
            .await
            .is_err(),
        "the verifying client must reject the self-signed cert"
    );
    let response = execute(&opted_in, &get(url), &Default::default())
        .await
        .expect("the insecure client must accept the self-signed cert");
    assert_eq!(response.status, 200);
}

fn build_client_insecure() -> reqwest::Client {
    churl_core::http::build_client_with(&ClientConfig {
        timeout: Duration::from_secs(5),
        insecure: true,
        ..Default::default()
    })
    .unwrap()
}
