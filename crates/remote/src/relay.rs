//! Bounded, authenticated routing only; no execution or offline command queue.
use crate::{Frame, MAX_FRAME};
use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{
        Path, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{Mutex, mpsc};

#[derive(Default, Serialize, Deserialize)]
pub struct Config {
    /// Device ID -> SHA-256 of an independently generated relay access token.
    pub devices: BTreeMap<String, String>,
}

pub fn token_hash(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

struct Connection {
    generation: String,
    tx: mpsc::Sender<Message>,
}

#[derive(Clone)]
struct Hub {
    config: PathBuf,
    connections: Arc<Mutex<BTreeMap<String, Connection>>>,
}

pub fn router(config: PathBuf) -> Router {
    Router::new()
        .route(
            "/health",
            get(|| async {
                axum::Json(serde_json::json!({"service":"trustd-relay","protocol":2}))
            }),
        )
        .route("/v2/relay/{device}", get(connect))
        .with_state(Hub {
            config,
            connections: Default::default(),
        })
}

fn authorized(path: &std::path::Path, device: &str, token: &str) -> bool {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice::<Config>(&b).ok())
        .and_then(|c| c.devices.get(device).cloned())
        .is_some_and(|expected| expected == token_hash(token))
}

fn announce_offline(peers: &BTreeMap<String, Connection>, device: &str) {
    for (id, peer) in peers.iter().filter(|(id, _)| id.as_str() != device) {
        let frame = Frame {
            from: device.into(),
            to: id.clone(),
            channel: uuid::Uuid::nil().to_string(),
            kind: "offline".into(),
            data: String::new(),
        };
        if let Ok(text) = serde_json::to_string(&frame) {
            let _ = peer.tx.try_send(Message::Text(text.into()));
        }
    }
}

async fn connect(
    State(hub): State<Hub>,
    Path(device): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_owned();
    if !authorized(&hub.config, &device, &token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    ws.max_message_size(MAX_FRAME)
        .max_frame_size(MAX_FRAME)
        .on_upgrade(move |socket| connection(hub, device, token, socket))
        .into_response()
}

async fn connection(hub: Hub, device: String, token: String, socket: WebSocket) {
    let generation = uuid::Uuid::new_v4().to_string();
    let (tx, mut rx) = mpsc::channel(64);
    {
        let mut peers = hub.connections.lock().await;
        if let Some(old) = peers.insert(
            device.clone(),
            Connection {
                generation: generation.clone(),
                tx: tx.clone(),
            },
        ) {
            let _ = old.tx.try_send(Message::Close(None));
            announce_offline(&peers, &device);
        }
    }
    let (mut sink, mut stream) = socket.split();
    let result: Result<()> = async {
        let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
        let mut last_seen = tokio::time::Instant::now();
        loop {
            tokio::select! {
                message = rx.recv() => {
                    let message = message.context("relay sender ended")?;
                    let closing = matches!(message, Message::Close(_));
                    tokio::time::timeout(Duration::from_secs(20), sink.send(message)).await??;
                    if closing { break; }
                }
                incoming = stream.next() => {
                    let incoming = incoming.context("device disconnected")??;
                    last_seen = tokio::time::Instant::now();
                    match incoming {
                        Message::Text(text) => {
                            let mut frame: Frame = serde_json::from_str(&text)?;
                            frame.validate()?;
                            anyhow::ensure!(frame.kind != "offline", "offline is a relay control message");
                            frame.from = device.clone(); // Never trust a claimed source identity.
                            let peers = hub.connections.lock().await;
                            if peers.get(&device).is_none_or(|p| p.generation != generation) { break; }
                            let destination = peers.get(&frame.to).map(|p| p.tx.clone());
                            drop(peers);
                            let forwarded = if let Some(destination) = destination {
                                destination.try_send(Message::Text(serde_json::to_string(&frame)?.into())).is_ok()
                            } else { false };
                            if !forwarded {
                                let reply = Frame { from: frame.to, to: device.clone(), channel: frame.channel, kind: "close".into(), data: "target unavailable or busy".into() };
                                // Do not wait on our own queue from its consumer.
                                sink.send(Message::Text(serde_json::to_string(&reply)?.into())).await?;
                            }
                        }
                        Message::Ping(data) => sink.send(Message::Pong(data)).await?,
                        Message::Pong(_) => {},
                        Message::Close(_) => break,
                        _ => anyhow::bail!("unexpected relay frame"),
                    }
                }
                _ = heartbeat.tick() => {
                    if last_seen.elapsed() > Duration::from_secs(70) || !authorized(&hub.config, &device, &token) { break; }
                    sink.send(Message::Ping(Vec::new().into())).await?;
                }
            }
        }
        Ok(())
    }.await;
    let mut peers = hub.connections.lock().await;
    if peers
        .get(&device)
        .is_some_and(|p| p.generation == generation)
    {
        peers.remove(&device);
        announce_offline(&peers, &device);
    }
    drop(peers);
    if let Err(error) = result {
        tracing::debug!(%device, %error, "relay connection closed");
    }
}
