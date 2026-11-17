use crate::{CHUNK, Frame, Identity, MAX_FRAME, MAX_PAYLOAD, PATTERN, node_id};
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, RwLock, Semaphore, mpsc};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message, handshake::derive_accept_key, protocol::Role},
};

pub type Handler =
    Arc<dyn Fn(String, Value) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>> + Send + Sync>;
/// Supplies the bearer token for each connection attempt, so a short-lived
/// access token is renewed before every reconnect.
pub type TokenSource =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Result<String>> + Send>> + Send + Sync>;

/// A token source that always returns the same token.
pub fn static_token(token: &str) -> TokenSource {
    let token = Arc::new(zeroize::Zeroizing::new(token.to_owned()));
    Arc::new(move || {
        let token = token.clone();
        Box::pin(async move { Ok(token.as_str().to_owned()) })
    })
}
type Inbox = mpsc::Receiver<Frame>;

pub struct Node {
    identity: Identity,
    peers: RwLock<BTreeMap<String, String>>,
    sender: RwLock<Option<mpsc::Sender<Frame>>>,
    channels: Mutex<BTreeMap<String, mpsc::Sender<Frame>>>,
    permits: Arc<Semaphore>,
    connected: AtomicBool,
}

impl Node {
    pub fn new(identity: Identity, peers: BTreeMap<String, String>) -> Arc<Self> {
        Arc::new(Self {
            identity,
            peers: RwLock::new(peers),
            sender: RwLock::new(None),
            channels: Mutex::new(BTreeMap::new()),
            permits: Arc::new(Semaphore::new(64)),
            connected: AtomicBool::new(false),
        })
    }

    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub async fn disconnect(&self) {
        self.connected.store(false, Ordering::Release);
        self.sender.write().await.take();
        self.channels.lock().await.clear();
    }

    pub async fn set_peers(&self, peers: BTreeMap<String, String>) {
        *self.peers.write().await = peers;
    }

    pub async fn run(self: Arc<Self>, url: String, token: TokenSource, handler: Handler) {
        let mut delay = 1;
        loop {
            let result = match token().await {
                Ok(token) => {
                    let token = zeroize::Zeroizing::new(token);
                    self.connection(&url, &token, handler.clone()).await
                }
                Err(error) => Err(error.context("relay access token unavailable")),
            };
            self.disconnect().await;
            if let Err(error) = result {
                tracing::warn!(%error, "remote relay disconnected");
            }
            tokio::time::sleep(Duration::from_secs(delay)).await;
            delay = (delay * 2).min(15);
        }
    }

    async fn connection(self: &Arc<Self>, url: &str, token: &str, handler: Handler) -> Result<()> {
        let mut url = reqwest::Url::parse(url)?;
        let scheme = match url.scheme() {
            "wss" => "https",
            "ws" => "http",
            "https" => "https",
            "http" => "http",
            _ => anyhow::bail!("relay must use wss or https"),
        };
        let local = url.host_str().is_some_and(|h| {
            h == "localhost"
                || h.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
        ensure!(
            scheme == "https" || local,
            "cleartext relay connections are allowed only on loopback"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "relay URL must not contain credentials, query or fragment"
        );
        url.set_scheme(scheme)
            .map_err(|_| anyhow::anyhow!("invalid relay scheme"))?;
        let base = url.path().trim_end_matches('/').to_owned();
        url.set_path(&format!("{base}/v2/relay/{}", self.identity.id()?));
        let key = STANDARD.encode(uuid::Uuid::new_v4().as_bytes());
        // reqwest handles HTTP CONNECT, environment and supported system proxies.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        let response = client
            .get(url)
            .bearer_auth(token)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", &key)
            .send()
            .await?;
        ensure!(
            response.status().as_u16() == 101,
            "relay rejected connection: HTTP {}",
            response.status().as_u16()
        );
        ensure!(
            response
                .headers()
                .get("sec-websocket-accept")
                .and_then(|v| v.to_str().ok())
                == Some(derive_accept_key(key.as_bytes()).as_str()),
            "invalid relay WebSocket handshake"
        );
        let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME))
            .max_frame_size(Some(MAX_FRAME));
        let socket =
            WebSocketStream::from_raw_socket(response.upgrade().await?, Role::Client, Some(config))
                .await;
        let (mut sink, mut stream) = socket.split();
        let (tx, mut rx) = mpsc::channel::<Frame>(64);
        *self.sender.write().await = Some(tx.clone());
        self.connected.store(true, Ordering::Release);
        let mut heartbeat = tokio::time::interval(Duration::from_secs(20));
        let mut last_seen = tokio::time::Instant::now();
        loop {
            tokio::select! {
                outgoing = rx.recv() => {
                    let outgoing = outgoing.context("node sender stopped")?;
                    tokio::time::timeout(Duration::from_secs(20), sink.send(Message::Text(serde_json::to_string(&outgoing)?.into()))).await??;
                }
                incoming = stream.next() => {
                    let message = incoming.context("relay closed connection")??;
                    last_seen = tokio::time::Instant::now();
                    match message {
                        Message::Text(text) => {
                            let frame: Frame = serde_json::from_str(&text)?;
                            frame.validate()?;
                            ensure!(frame.to == self.identity.id()?, "relay destination mismatch");
                            if frame.kind == "offline" {
                                let prefix = format!("{}/", frame.from);
                                self.channels.lock().await.retain(|key, _| !key.starts_with(&prefix));
                                continue;
                            }
                            let channel_key = format!("{}/{}", frame.from, frame.channel);
                            let mut channels = self.channels.lock().await;
                            if frame.kind == "open" {
                                if channels.contains_key(&channel_key) { continue; }
                                let Some(public) = self.peers.read().await.get(&frame.from).cloned() else { continue; };
                                let Ok(permit) = self.permits.clone().try_acquire_owned() else { continue; };
                                let (in_tx, inbox) = mpsc::channel(64);
                                in_tx.try_send(frame.clone())?;
                                channels.insert(channel_key.clone(), in_tx);
                                let node = self.clone(); let handler = handler.clone(); let tx = tx.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    let peer = frame.from.clone(); let channel = frame.channel.clone();
                                    if let Err(error) = node.respond(frame, public, inbox, tx.clone(), handler).await { tracing::debug!(%error, "remote request ended"); }
                                    node.channels.lock().await.remove(&channel_key);
                                    let _ = tx.send(Frame { from: String::new(), to: peer, channel, kind: "close".into(), data: String::new() }).await;
                                });
                            } else if let Some(sender) = channels.get(&channel_key)
                                && sender.try_send(frame).is_err() { channels.remove(&channel_key); }
                        }
                        Message::Ping(data) => sink.send(Message::Pong(data)).await?,
                        Message::Pong(_) => {},
                        Message::Close(_) => return Ok(()),
                        _ => anyhow::bail!("invalid relay message"),
                    }
                }
                _ = heartbeat.tick() => {
                    ensure!(last_seen.elapsed() < Duration::from_secs(70), "relay heartbeat lost");
                    sink.send(Message::Ping(Vec::new().into())).await?;
                }
            }
        }
    }

    pub async fn call(&self, peer: &str, request: Value) -> Result<Value> {
        let _permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .context("too many remote requests")?;
        let public = self
            .peers
            .read()
            .await
            .get(peer)
            .cloned()
            .context("device is not paired")?;
        let tx = self
            .sender
            .read()
            .await
            .clone()
            .context("relay is offline; request not submitted")?;
        let channel = uuid::Uuid::new_v4().to_string();
        let key = format!("{peer}/{channel}");
        let (sender, mut inbox) = mpsc::channel(64);
        self.channels.lock().await.insert(key.clone(), sender);
        let result = async {
            let private = zeroize::Zeroizing::new(STANDARD.decode(&self.identity.private_key)?);
            let public = STANDARD.decode(public)?;
            let mut noise = snow::Builder::new(PATTERN.parse()?)
                .local_private_key(&private)?
                .remote_public_key(&public)?
                .build_initiator()?;
            let mut buffer = vec![0; 65535];
            let size = noise.write_message(&[], &mut buffer)?;
            send_frame(&tx, peer, &channel, "open", &buffer[..size]).await?;
            let reply =
                tokio::time::timeout(Duration::from_secs(20), receive(&mut inbox)).await??;
            ensure!(
                noise.read_message(&reply, &mut buffer)? == 0,
                "unexpected handshake payload"
            );
            let mut transport = noise.into_transport_mode()?;
            send_value(&tx, peer, &channel, &mut transport, &request).await?;
            // Execution has no default deadline. A dropped transport reports an unknown outcome.
            let response = read_value(&mut inbox, &mut transport).await?;
            if let Some(error) = response.get("error").and_then(Value::as_str) {
                anyhow::bail!("{error}");
            }
            response
                .get("ok")
                .cloned()
                .context("invalid remote response")
        }
        .await;
        self.channels.lock().await.remove(&key);
        result
    }

    async fn respond(
        &self,
        first: Frame,
        public: String,
        mut inbox: Inbox,
        tx: mpsc::Sender<Frame>,
        handler: Handler,
    ) -> Result<()> {
        let private = zeroize::Zeroizing::new(STANDARD.decode(&self.identity.private_key)?);
        let expected = STANDARD.decode(public)?;
        ensure!(
            node_id(&STANDARD.encode(&expected))? == first.from,
            "peer identity mismatch"
        );
        let mut noise = snow::Builder::new(PATTERN.parse()?)
            .local_private_key(&private)?
            .build_responder()?;
        let mut buffer = vec![0; 65535];
        let bytes = receive(&mut inbox).await?;
        ensure!(
            noise.read_message(&bytes, &mut buffer)? == 0,
            "requests must follow the completed handshake"
        );
        ensure!(
            noise.get_remote_static() == Some(expected.as_slice()),
            "unpaired device key"
        );
        let size = noise.write_message(&[], &mut buffer)?;
        send_frame(&tx, &first.from, &first.channel, "data", &buffer[..size]).await?;
        let mut transport = noise.into_transport_mode()?;
        let request = tokio::time::timeout(
            Duration::from_secs(30),
            read_value(&mut inbox, &mut transport),
        )
        .await??;
        let response = match handler(first.from.clone(), request).await {
            Ok(value) => json!({"ok":value}),
            Err(error) => json!({"error":error.to_string()}),
        };
        send_value(&tx, &first.from, &first.channel, &mut transport, &response).await
    }
}

async fn send_frame(
    tx: &mpsc::Sender<Frame>,
    peer: &str,
    channel: &str,
    kind: &str,
    bytes: &[u8],
) -> Result<()> {
    tx.send(Frame {
        from: String::new(),
        to: peer.into(),
        channel: channel.into(),
        kind: kind.into(),
        data: STANDARD.encode(bytes),
    })
    .await
    .context("relay disconnected; outcome may be unknown")
}

async fn receive(inbox: &mut Inbox) -> Result<Vec<u8>> {
    let frame = inbox
        .recv()
        .await
        .context("connection interrupted; outcome may be unknown; query before retrying")?;
    ensure!(
        frame.kind != "close",
        "remote channel closed; query task status before retrying"
    );
    Ok(STANDARD.decode(frame.data)?)
}

async fn send_value(
    tx: &mpsc::Sender<Frame>,
    peer: &str,
    channel: &str,
    cipher: &mut snow::TransportState,
    value: &Value,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_PAYLOAD,
        "remote response too large; use pagination or file chunks"
    );
    let mut payload = (bytes.len() as u32).to_be_bytes().to_vec();
    payload.extend(bytes);
    let mut buffer = vec![0; 65535];
    for chunk in payload.chunks(CHUNK) {
        let size = cipher.write_message(chunk, &mut buffer)?;
        send_frame(tx, peer, channel, "data", &buffer[..size]).await?;
    }
    Ok(())
}

async fn read_value(inbox: &mut Inbox, cipher: &mut snow::TransportState) -> Result<Value> {
    let mut payload = zeroize::Zeroizing::new(Vec::new());
    let mut length = None;
    let mut buffer = zeroize::Zeroizing::new(vec![0; 65535]);
    loop {
        let bytes = receive(inbox).await?;
        let size = cipher.read_message(&bytes, &mut buffer)?;
        payload.extend_from_slice(&buffer[..size]);
        if length.is_none() && payload.len() >= 4 {
            let total = u32::from_be_bytes(payload[..4].try_into()?) as usize;
            ensure!(total <= MAX_PAYLOAD, "remote payload too large");
            length = Some(total + 4);
        }
        ensure!(payload.len() <= MAX_PAYLOAD + 4, "remote payload too large");
        if let Some(length) = length {
            ensure!(payload.len() <= length, "invalid message framing");
            if payload.len() == length {
                return Ok(serde_json::from_slice(&payload[4..])?);
            }
        }
    }
}
