//! WebSocket channel tests against a local echo server (the same shape the
//! relay exposes: one binary or text frame in, the same frame back).

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;

use syncengine_transport::channel::{FrameIo, WsIo};

async fn spawn_echo_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };
                while let Some(Ok(message)) = socket.next().await {
                    let echo = match message {
                        tokio_tungstenite::tungstenite::Message::Binary(_)
                        | tokio_tungstenite::tungstenite::Message::Text(_) => message,
                        tokio_tungstenite::tungstenite::Message::Close(_) => break,
                        _ => continue,
                    };
                    if socket.send(echo).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    format!("ws://{addr}")
}

#[tokio::test]
async fn websocket_channel_round_trips_binary_frames() {
    let url = spawn_echo_server().await;
    let (socket, _) = tokio_tungstenite::connect_async(&url)
        .await
        .expect("connect");
    let mut io = WsIo::new(socket);

    io.send(b"hello relay").await.expect("send");
    let echoed = io.recv().await.expect("recv").expect("frame");
    assert_eq!(echoed, b"hello relay");

    let big = vec![7u8; 200 * 1024];
    io.send(&big).await.expect("send big");
    let echoed = io.recv().await.expect("recv").expect("frame");
    assert_eq!(echoed, big);

    drop(io);
}

#[tokio::test]
async fn closed_websocket_yields_none() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut socket = tokio_tungstenite::accept_async(stream).await.expect("ws");
        let _ = socket.close(None).await;
    });

    let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}"))
        .await
        .expect("connect");
    let mut io = WsIo::new(socket);
    assert!(io.recv().await.expect("clean close").is_none());
}
