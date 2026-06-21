use parrot_transport::{TransportClient, TransportServer, WsTransportClient, WsTransportServer, accept_connection};
use parrot_protocol::{ClientMessage, ServerMessage};
use tokio::time::{timeout, Duration};

/// Bind to an ephemeral port to avoid conflicts between tests.
async fn bind_ephemeral() -> (tokio::net::TcpListener, u16) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind failed");
    let port = listener.local_addr().expect("local addr").port();
    (listener, port)
}

#[tokio::test]
async fn ws_server_client_message_roundtrip() {
    let server = WsTransportServer::new("127.0.0.1", 19877);

    // Bind the server
    let listener = server.bind().await.expect("bind failed");

    // Spawn server accept in background
    let server_handle = tokio::spawn(async move {
        let conn = accept_connection(&listener).await.expect("accept failed");
        conn
    });

    // Give server time to start listening
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = WsTransportClient::new();
    let mut conn = client
        .connect("ws://127.0.0.1:19877", "test-token-123")
        .await
        .expect("connect failed");

    // The client auto-sends Hello, server should receive it
    let mut server_conn = server_handle.await.expect("server task failed");

    // Wait for the Hello message with timeout
    let msg = timeout(Duration::from_secs(3), server_conn.receiver.recv())
        .await
        .expect("timed out waiting for Hello from client");

    match msg {
        Some(ClientMessage::Hello { token, .. }) => {
            assert_eq!(token, "test-token-123");
        }
        Some(other) => panic!("Expected Hello, got: {:?}", other),
        None => panic!("Channel closed, no message received from client"),
    }

    // Server sends HelloAck to client
    server_conn
        .sender
        .send(ServerMessage::HelloAck {
            server_version: "0.1.0".into(),
        })
        .await
        .expect("send failed");

    // Client receives HelloAck with timeout
    let msg = timeout(Duration::from_secs(3), conn.receiver.recv())
        .await
        .expect("timed out waiting for HelloAck from server");

    match msg {
        Some(ServerMessage::HelloAck { server_version }) => {
            assert_eq!(server_version, "0.1.0");
        }
        Some(other) => panic!("Expected HelloAck, got: {:?}", other),
        None => panic!("Channel closed, no HelloAck received from server"),
    }
}

/// A client presenting a cross-origin `Origin` header (e.g. from a malicious
/// web page) must be rejected during the WS handshake: `accept_connection`
/// returns `Err(OriginRejected)` and the client's connection fails.
#[tokio::test]
async fn ws_server_rejects_cross_origin() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let (listener, port) = bind_ephemeral().await;

    let server_handle = tokio::spawn(async move {
        accept_connection(&listener).await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Build a WS request with a non-local Origin (simulating a browser CSRF).
    let url = format!("ws://127.0.0.1:{port}/");
    let mut req = url.into_client_request().expect("build request");
    req.headers_mut().insert(
        "Origin",
        "http://evil.example.com".parse::<HeaderValue>().expect("header value"),
    );

    // The client side should also fail to connect (handshake rejected with 403).
    let client_result = tokio_tungstenite::connect_async(req).await;

    let server_result = timeout(Duration::from_secs(3), server_handle)
        .await
        .expect("server accept timed out")
        .expect("server task panicked");

    // Server must report an OriginRejected error.
    match server_result {
        Err(parrot_transport::TransportError::OriginRejected(_)) => {}
        other => panic!("expected OriginRejected, got Ok or other error: {:?}", other.err()),
    }

    // Client must have failed too (either an HTTP error or a connection drop).
    assert!(
        client_result.is_err(),
        "cross-origin client should not complete the handshake, got {:?}",
        client_result.ok()
    );
}

/// A client presenting a localhost `Origin` header is allowed (e.g. a
/// browser-based local UI). The handshake completes normally.
#[tokio::test]
async fn ws_server_allows_localhost_origin() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let (listener, port) = bind_ephemeral().await;

    let server_handle = tokio::spawn(async move {
        accept_connection(&listener).await
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let url = format!("ws://127.0.0.1:{port}/");
    let mut req = url.into_client_request().expect("build request");
    let origin = "http://localhost:5173"
        .parse::<HeaderValue>()
        .expect("header value");
    req.headers_mut().insert("Origin", origin);

    let _ws = tokio_tungstenite::connect_async(req)
        .await
        .expect("localhost origin should be accepted");

    let server_conn = timeout(Duration::from_secs(3), server_handle)
        .await
        .expect("server accept timed out")
        .expect("server task panicked")
        .expect("accept should succeed for localhost origin");

    // The server-side ClientConnection is usable (id is a fresh uuid).
    assert!(server_conn.id != uuid::Uuid::nil());
}