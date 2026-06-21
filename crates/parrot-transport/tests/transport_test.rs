use parrot_transport::{TransportClient, TransportServer, WsTransportClient, WsTransportServer, accept_connection};
use parrot_protocol::{ClientMessage, ServerMessage};
use tokio::time::{timeout, Duration};

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