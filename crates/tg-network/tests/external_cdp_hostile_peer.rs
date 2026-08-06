//! Public CDP adapter coverage against a bounded, local hostile WebSocket peer.
//!
//! This deliberately does not start a browser. It exercises only the public `tg_network`
//! adapter boundary and keeps the fixture's listener, handshake, reads, writes, and cleanup
//! bounded so a stalled or speculative peer cannot leave the test hanging.

use std::collections::VecDeque;
use std::error::Error;
use std::io;
use std::net::Ipv4Addr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tg_network::{CdpError, CdpLimits, CdpSession};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::{WebSocketStream, accept_async_with_config};

type TestError = Box<dyn Error + Send + Sync>;
type TestResult<T = ()> = Result<T, TestError>;
type PeerSocket = WebSocketStream<TcpStream>;

const LISTENER_ACCEPT_DEADLINE: Duration = Duration::from_secs(2);
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(1);
const PEER_READ_DEADLINE: Duration = Duration::from_secs(1);
const PEER_WRITE_DEADLINE: Duration = Duration::from_secs(1);
const FIXTURE_TASK_DEADLINE: Duration = Duration::from_secs(5);
const NORMAL_OPERATION_DEADLINE: Duration = Duration::from_secs(1);
const TIMEOUT_OPERATION_DEADLINE: Duration = Duration::from_millis(200);
const CALLER_CANCELLATION_DEADLINE: Duration = Duration::from_millis(75);
const MAX_ACCEPTED_CONNECTIONS: usize = 16;
const MAX_IGNORED_CONTROL_FRAMES: usize = 4;
const MAX_SCRIPTED_REQUEST_BYTES: usize = 4 * 1024;
const MAX_SCRIPTED_RESPONSE_BYTES: usize = 1024;
const OVERSIZED_MESSAGE_BYTES: usize = 512;
const SMALL_MESSAGE_LIMIT: usize = 128;

#[derive(Clone, Copy, Debug)]
enum PeerScript {
    Normal,
    OversizedMessage,
    MalformedJson,
    FutureResponse,
    EventOverflow,
    StaleResponseOverflow,
    HoldForClientCleanup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerProgress {
    Empty,
    Consumed,
}

struct HostilePeer {
    endpoint: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<TestResult<usize>>>,
}

impl HostilePeer {
    async fn start(scripts: Vec<PeerScript>) -> TestResult<Self> {
        if scripts.is_empty() {
            return Err(failure("hostile peer requires at least one script"));
        }

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let endpoint = format!(
            "ws://{}/devtools/browser/hostile-peer",
            listener.local_addr()?
        );
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(serve_scripts(listener, shutdown_receiver, scripts));

        Ok(Self {
            endpoint,
            shutdown: Some(shutdown),
            task: Some(task),
        })
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn shutdown(&mut self) -> TestResult {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let task = self
            .task
            .take()
            .ok_or_else(|| failure("hostile peer task was already joined"))?;
        let joined = timeout(FIXTURE_TASK_DEADLINE, task)
            .await
            .map_err(|_| failure("hostile peer task did not stop before its deadline"))?;
        let result = joined?;
        let accepted = result?;
        if accepted == 0 {
            return Err(failure(
                "hostile peer did not service a scripted connection",
            ));
        }
        Ok(())
    }
}

impl Drop for HostilePeer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn serve_scripts(
    listener: TcpListener,
    mut shutdown: oneshot::Receiver<()>,
    scripts: Vec<PeerScript>,
) -> TestResult<usize> {
    let mut scripts: VecDeque<_> = scripts.into();
    let mut accepted_connections = 0;

    while let Some(script) = scripts.front().copied() {
        if accepted_connections >= MAX_ACCEPTED_CONNECTIONS {
            return Err(failure(format!(
                "hostile peer exceeded its {MAX_ACCEPTED_CONNECTIONS} connection bound"
            )));
        }

        let accepted = tokio::select! {
            _ = &mut shutdown => {
                return Err(failure(format!(
                    "hostile peer stopped with {} unconsumed script(s)",
                    scripts.len()
                )));
            }
            accepted = timeout(LISTENER_ACCEPT_DEADLINE, listener.accept()) => accepted,
        };
        let accepted = accepted
            .map_err(|_| failure("hostile peer listener timed out waiting for a connection"))?;
        let (stream, _) = accepted?;
        accepted_connections += 1;

        // A browser discovery probe can connect and disappear before completing a WebSocket
        // handshake. It is not a test failure and must not consume the next scripted action.
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_SCRIPTED_REQUEST_BYTES))
            .max_frame_size(Some(MAX_SCRIPTED_REQUEST_BYTES));
        let mut socket = match timeout(
            HANDSHAKE_DEADLINE,
            accept_async_with_config(stream, Some(config)),
        )
        .await
        {
            Ok(Ok(socket)) => socket,
            Ok(Err(_)) | Err(_) => continue,
        };

        if serve_script(&mut socket, script).await? == PeerProgress::Consumed {
            let _ = scripts.pop_front();
        }
    }

    Ok(accepted_connections)
}

async fn serve_script(socket: &mut PeerSocket, script: PeerScript) -> TestResult<PeerProgress> {
    let Some(request) = receive_request(socket).await? else {
        // The peer completed a handshake but sent no command. Preserve the script for the next
        // connection rather than treating this speculative connection as fatal.
        return Ok(PeerProgress::Empty);
    };
    let id = request_id(&request)?;
    let method = request_method(&request)?;

    match script {
        PeerScript::Normal => {
            send_json(
                socket,
                json!({ "id": id, "result": { "accepted": true, "method": method } }),
            )
            .await?;
        }
        PeerScript::OversizedMessage => {
            send_text(socket, "x".repeat(OVERSIZED_MESSAGE_BYTES)).await?;
        }
        PeerScript::MalformedJson => {
            send_text(socket, "{not-json".to_owned()).await?;
        }
        PeerScript::FutureResponse => {
            let future_id = id
                .checked_add(1)
                .ok_or_else(|| failure("test request ID unexpectedly overflowed"))?;
            send_json(
                socket,
                json!({ "id": future_id, "result": { "unexpected": true } }),
            )
            .await?;
        }
        PeerScript::EventOverflow => {
            send_json(
                socket,
                json!({ "method": "Peer.first", "params": { "sequence": 1 } }),
            )
            .await?;
            send_json(
                socket,
                json!({ "method": "Peer.second", "params": { "sequence": 2 } }),
            )
            .await?;
        }
        PeerScript::StaleResponseOverflow => {
            send_json(
                socket,
                json!({ "id": id, "result": { "accepted": true, "phase": "first" } }),
            )
            .await?;
            let second_request = receive_request(socket)
                .await?
                .ok_or_else(|| failure("client did not issue the second correlation request"))?;
            let second_id = request_id(&second_request)?;
            if second_id <= id {
                return Err(failure(
                    "client did not advance its request ID before the stale response test",
                ));
            }
            send_json(socket, json!({ "id": id, "result": { "stale": true } })).await?;
            send_json(socket, json!({ "id": id, "result": { "duplicate": true } })).await?;
        }
        PeerScript::HoldForClientCleanup => {}
    }

    wait_for_client_cleanup(socket).await?;
    Ok(PeerProgress::Consumed)
}

async fn receive_request(socket: &mut PeerSocket) -> TestResult<Option<Value>> {
    for _ in 0..MAX_IGNORED_CONTROL_FRAMES {
        let message = match timeout(PEER_READ_DEADLINE, socket.next()).await {
            // An empty or stalled connection is a non-fatal speculative peer. The outer server
            // loop keeps the same script and accepts another bounded connection.
            Err(_) | Ok(None) | Ok(Some(Err(_))) => return Ok(None),
            Ok(Some(Ok(message))) => message,
        };
        match message {
            Message::Text(text) => {
                let text = text.to_string();
                if text.len() > MAX_SCRIPTED_REQUEST_BYTES {
                    return Err(failure("client request exceeded the fixture byte bound"));
                }
                return serde_json::from_str(&text)
                    .map(Some)
                    .map_err(|_| failure("client sent non-JSON CDP request"));
            }
            Message::Ping(payload) => send_frame(socket, Message::Pong(payload)).await?,
            Message::Pong(_) => {}
            Message::Close(_) => return Ok(None),
            Message::Binary(_) | Message::Frame(_) => {
                return Err(failure("client sent an unexpected non-text CDP request"));
            }
        }
    }
    Err(failure("client exceeded the fixture control-frame bound"))
}

async fn wait_for_client_cleanup(socket: &mut PeerSocket) -> TestResult {
    for _ in 0..MAX_IGNORED_CONTROL_FRAMES {
        let message = match timeout(PEER_READ_DEADLINE, socket.next()).await {
            // A hostile response can make the public client drop the TCP connection without a
            // close frame. That is still successful cleanup for this one-shot peer action.
            Err(_) | Ok(None) | Ok(Some(Err(_))) => return Ok(()),
            Ok(Some(Ok(message))) => message,
        };
        match message {
            Message::Close(_) => return Ok(()),
            Message::Ping(payload) => send_frame(socket, Message::Pong(payload)).await?,
            Message::Pong(_) => {}
            Message::Text(_) | Message::Binary(_) | Message::Frame(_) => {
                return Err(failure(
                    "client sent a second command before closing the peer",
                ));
            }
        }
    }
    Err(failure(
        "client did not complete cleanup within the frame bound",
    ))
}

async fn send_json(socket: &mut PeerSocket, value: Value) -> TestResult {
    send_text(socket, serde_json::to_string(&value)?).await
}

async fn send_text(socket: &mut PeerSocket, text: String) -> TestResult {
    if text.len() > MAX_SCRIPTED_RESPONSE_BYTES {
        return Err(failure(
            "hostile peer response exceeded its fixture byte bound",
        ));
    }
    send_frame(socket, Message::Text(text.into())).await
}

async fn send_frame(socket: &mut PeerSocket, message: Message) -> TestResult {
    let write = timeout(PEER_WRITE_DEADLINE, socket.send(message))
        .await
        .map_err(|_| failure("hostile peer write exceeded its deadline"))?;
    write?;
    Ok(())
}

fn request_id(request: &Value) -> TestResult<u64> {
    request
        .get("id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| failure("client request omitted a positive numeric ID"))
}

fn request_method(request: &Value) -> TestResult<String> {
    request
        .get("method")
        .and_then(Value::as_str)
        .filter(|method| !method.is_empty() && method.len() <= MAX_SCRIPTED_REQUEST_BYTES)
        .map(ToOwned::to_owned)
        .ok_or_else(|| failure("client request omitted a bounded method"))
}

fn peer_limits() -> CdpLimits {
    CdpLimits {
        max_message_bytes: 1024,
        max_events: 4,
        max_pending_responses: 4,
        operation_timeout: NORMAL_OPERATION_DEADLINE,
        ..CdpLimits::default()
    }
}

async fn finish_fixture(fixture: &mut HostilePeer, operation: TestResult) -> TestResult {
    let cleanup = fixture.shutdown().await;
    operation?;
    cleanup
}

fn failure(message: impl Into<String>) -> TestError {
    io::Error::other(message.into()).into()
}

#[tokio::test]
async fn public_cdp_rejects_non_loopback_and_credentialed_endpoints() -> TestResult {
    for endpoint in [
        "ws://192.0.2.1:9222/devtools/browser/hostile-peer",
        "ws://user:secret@127.0.0.1:9222/devtools/browser/hostile-peer",
    ] {
        assert!(matches!(
            CdpSession::connect(endpoint, peer_limits()).await,
            Err(CdpError::InvalidEndpoint)
        ));
    }
    Ok(())
}

#[tokio::test]
async fn public_cdp_rejects_hostile_messages_correlation_and_queue_overflow() -> TestResult {
    let mut peer = HostilePeer::start(vec![
        PeerScript::Normal,
        PeerScript::OversizedMessage,
        PeerScript::MalformedJson,
        PeerScript::FutureResponse,
        PeerScript::EventOverflow,
        PeerScript::StaleResponseOverflow,
    ])
    .await?;
    let endpoint = peer.endpoint().to_owned();

    let operation = async {
        let mut normal = CdpSession::connect(&endpoint, peer_limits()).await?;
        let reply = normal
            .send("TermGlide.normal", json!({ "mode": "baseline" }))
            .await?;
        assert_eq!(reply.id, 1);
        assert_eq!(
            reply.result,
            json!({ "accepted": true, "method": "TermGlide.normal" })
        );
        assert_eq!(normal.queued_event_count(), 0);
        normal.close().await?;

        let mut oversized_limits = peer_limits();
        oversized_limits.max_message_bytes = SMALL_MESSAGE_LIMIT;
        let mut oversized = CdpSession::connect(&endpoint, oversized_limits).await?;
        assert!(matches!(
            oversized.send("TermGlide.oversized", Value::Null).await,
            Err(CdpError::MessageTooLarge)
        ));
        oversized.close().await?;

        let mut malformed = CdpSession::connect(&endpoint, peer_limits()).await?;
        assert!(matches!(
            malformed.send("TermGlide.malformed", Value::Null).await,
            Err(CdpError::MalformedMessage)
        ));
        malformed.close().await?;

        let mut wrong_response = CdpSession::connect(&endpoint, peer_limits()).await?;
        assert!(matches!(
            wrong_response
                .send("TermGlide.correlation", Value::Null)
                .await,
            Err(CdpError::MalformedMessage)
        ));
        wrong_response.close().await?;

        let mut event_limits = peer_limits();
        event_limits.max_events = 1;
        let mut event_overflow = CdpSession::connect(&endpoint, event_limits).await?;
        assert!(matches!(
            event_overflow
                .send("TermGlide.eventOverflow", Value::Null)
                .await,
            Err(CdpError::EventLimit { limit: 1 })
        ));
        assert_eq!(event_overflow.queued_event_count(), 1);
        event_overflow.close().await?;

        let mut pending_limits = peer_limits();
        pending_limits.max_pending_responses = 1;
        let mut pending_overflow = CdpSession::connect(&endpoint, pending_limits).await?;
        let first = pending_overflow
            .send("TermGlide.pendingFirst", Value::Null)
            .await?;
        assert_eq!(first.id, 1);
        assert!(matches!(
            pending_overflow
                .send("TermGlide.pendingSecond", Value::Null)
                .await,
            Err(CdpError::PendingResponseLimit { limit: 1 })
        ));
        pending_overflow.close().await?;
        Ok(())
    }
    .await;

    finish_fixture(&mut peer, operation).await
}

#[tokio::test]
async fn public_cdp_timeout_drop_cancellation_and_reconnect_use_fresh_transport() -> TestResult {
    let mut peer = HostilePeer::start(vec![
        PeerScript::HoldForClientCleanup,
        PeerScript::HoldForClientCleanup,
        PeerScript::Normal,
        PeerScript::Normal,
    ])
    .await?;
    let endpoint = peer.endpoint().to_owned();

    let operation = async {
        let mut timeout_limits = peer_limits();
        timeout_limits.operation_timeout = TIMEOUT_OPERATION_DEADLINE;
        let mut timed_out = CdpSession::connect(&endpoint, timeout_limits).await?;
        assert!(matches!(
            timed_out.send("TermGlide.timeout", Value::Null).await,
            Err(CdpError::Timeout)
        ));
        timed_out.close().await?;

        let mut cancellation_limits = peer_limits();
        cancellation_limits.operation_timeout = Duration::from_secs(2);
        let mut cancelled = CdpSession::connect(&endpoint, cancellation_limits).await?;
        // The public adapter exposes cancellation by dropping an in-flight operation, not by a
        // separate cancellation error. An outer timeout performs that drop deterministically.
        assert!(
            timeout(
                CALLER_CANCELLATION_DEADLINE,
                cancelled.send("TermGlide.cancelled", Value::Null)
            )
            .await
            .is_err()
        );
        assert!(cancelled.is_connected());
        cancelled.close().await?;

        let mut recovered = CdpSession::connect(&endpoint, peer_limits()).await?;
        let first = recovered
            .send("TermGlide.beforeReconnect", Value::Null)
            .await?;
        assert_eq!(first.id, 1);
        recovered.close().await?;
        assert!(!recovered.is_connected());

        recovered.reconnect().await?;
        assert!(recovered.is_connected());
        let second = recovered
            .send("TermGlide.afterReconnect", Value::Null)
            .await?;
        assert_eq!(second.id, 2);
        assert_eq!(
            second.result,
            json!({ "accepted": true, "method": "TermGlide.afterReconnect" })
        );
        recovered.close().await?;
        Ok(())
    }
    .await;

    finish_fixture(&mut peer, operation).await
}
