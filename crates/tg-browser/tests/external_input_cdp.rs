use std::error::Error;
use std::io;
use std::net::Ipv4Addr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tg_browser::{ExternalInputAdapter, ExternalInputError, ExternalInputGeometry};
use tg_core::Cancellation;
use tg_network::{CdpLimits, CdpSession};
use tg_terminal::{InputEvent, KeyCode, KeyEventKind, Modifiers, MouseButton};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::protocol::Message;

const SESSION_ID: &str = "external-input-regression";
const TEST_TIMEOUT: Duration = Duration::from_secs(2);

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
type MockSocket = WebSocketStream<TcpStream>;

fn adapter() -> TestResult<ExternalInputAdapter> {
    Ok(ExternalInputAdapter::new(ExternalInputGeometry::new(
        10, 4, 1000.0, 400.0,
    )?)?)
}

fn cdp_limits() -> CdpLimits {
    CdpLimits {
        operation_timeout: TEST_TIMEOUT,
        ..CdpLimits::default()
    }
}

async fn start_mock(expected_requests: Vec<Value>) -> TestResult<(String, JoinHandle<TestResult>)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        let (stream, _) = timeout(TEST_TIMEOUT, listener.accept()).await??;
        let socket = timeout(TEST_TIMEOUT, accept_async(stream)).await??;
        serve_requests(socket, expected_requests).await
    });
    Ok((
        format!("ws://{address}/devtools/page/external-input"),
        server,
    ))
}

async fn serve_requests(mut socket: MockSocket, expected_requests: Vec<Value>) -> TestResult {
    for expected in expected_requests {
        let actual = receive_json(&mut socket).await?;
        if actual != expected {
            return Err(io::Error::other(format!(
                "unexpected CDP request: expected {expected}, received {actual}"
            ))
            .into());
        }
        send_success(&mut socket, &actual).await?;
    }
    wait_for_client_close(&mut socket).await
}

async fn receive_json(socket: &mut MockSocket) -> TestResult<Value> {
    loop {
        let message = timeout(TEST_TIMEOUT, socket.next())
            .await?
            .ok_or_else(|| io::Error::other("mock client disconnected"))??;
        match message {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Ping(_) | Message::Pong(_) => socket.flush().await?,
            Message::Close(_) => return Err(io::Error::other("mock client closed early").into()),
            Message::Binary(_) | Message::Frame(_) => {
                return Err(io::Error::other("mock received an unexpected frame").into());
            }
        }
    }
}

fn request_id(request: &Value) -> TestResult<u64> {
    request
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| io::Error::other("request omitted a numeric id").into())
}

async fn send_success(socket: &mut MockSocket, request: &Value) -> TestResult {
    let id = request_id(request)?;
    socket
        .send(Message::Text(
            json!({ "id": id, "result": {} }).to_string().into(),
        ))
        .await?;
    Ok(())
}

async fn wait_for_client_close(socket: &mut MockSocket) -> TestResult {
    loop {
        let message = timeout(TEST_TIMEOUT, socket.next())
            .await?
            .ok_or_else(|| io::Error::other("mock client disconnected before close"))??;
        match message {
            Message::Close(_) => return Ok(()),
            Message::Ping(_) | Message::Pong(_) => socket.flush().await?,
            Message::Text(text) => {
                return Err(io::Error::other(format!(
                    "mock received an unexpected request before close: {text}"
                ))
                .into());
            }
            Message::Binary(_) | Message::Frame(_) => {
                return Err(io::Error::other("mock received an unexpected frame").into());
            }
        }
    }
}

async fn finish_server(server: JoinHandle<TestResult>) -> TestResult {
    let result = timeout(TEST_TIMEOUT, server).await?;
    result??;
    Ok(())
}

#[tokio::test]
async fn external_input_dispatches_flattened_target_actions_in_order() -> TestResult {
    let expected_requests = vec![
        json!({
            "id": 1,
            "method": "Input.dispatchKeyEvent",
            "params": {
                "type": "keyDown",
                "key": "X",
                "code": "KeyX",
                "windowsVirtualKeyCode": 88,
                "modifiers": 9,
                "autoRepeat": false,
            },
            "sessionId": SESSION_ID,
        }),
        json!({
            "id": 2,
            "method": "Input.insertText",
            "params": { "text": "X" },
            "sessionId": SESSION_ID,
        }),
        json!({
            "id": 3,
            "method": "Input.dispatchMouseEvent",
            "params": {
                "type": "mousePressed",
                "x": 350.0,
                "y": 150.0,
                "button": "middle",
                "clickCount": 1,
                "deltaX": 0.0,
                "deltaY": 0.0,
                "modifiers": 2,
            },
            "sessionId": SESSION_ID,
        }),
        json!({
            "id": 4,
            "method": "Input.dispatchMouseEvent",
            "params": {
                "type": "mouseWheel",
                "x": 50.0,
                "y": 50.0,
                "button": "none",
                "clickCount": 0,
                "deltaX": 0.0,
                "deltaY": 120.0,
                "modifiers": 0,
            },
            "sessionId": SESSION_ID,
        }),
        json!({
            "id": 5,
            "method": "Input.dispatchMouseEvent",
            "params": {
                "type": "mouseWheel",
                "x": 50.0,
                "y": 50.0,
                "button": "none",
                "clickCount": 0,
                "deltaX": 120.0,
                "deltaY": 0.0,
                "modifiers": 0,
            },
            "sessionId": SESSION_ID,
        }),
        json!({
            "id": 6,
            "method": "Emulation.setFocusEmulationEnabled",
            "params": { "enabled": true },
            "sessionId": SESSION_ID,
        }),
    ];
    let (endpoint, server) = start_mock(expected_requests).await?;
    let mut session = CdpSession::connect(endpoint, cdp_limits()).await?;
    let input_adapter = adapter()?;
    let cancellation = Cancellation::new();

    {
        let mut target = session.session(SESSION_ID)?;
        let shifted_key = InputEvent::Key {
            code: KeyCode::Character('x'),
            modifiers: Modifiers::ALT | Modifiers::SHIFT,
            kind: KeyEventKind::Press,
            shifted_key: Some('X'),
            base_layout_key: None,
            text: Some("X".to_owned()),
        };
        assert_eq!(
            input_adapter
                .dispatch(&mut target, &shifted_key, &cancellation)
                .await?
                .sent_actions(),
            2
        );

        let pointer = InputEvent::Mouse {
            button: MouseButton::Middle,
            column: 3,
            row: 1,
            pressed: true,
            modifiers: Modifiers::CONTROL,
        };
        assert_eq!(
            input_adapter
                .dispatch(&mut target, &pointer, &cancellation)
                .await?
                .sent_actions(),
            1
        );

        let wheel = InputEvent::Mouse {
            button: MouseButton::WheelDown,
            column: 0,
            row: 0,
            pressed: true,
            modifiers: Modifiers::empty(),
        };
        assert_eq!(
            input_adapter
                .dispatch(&mut target, &wheel, &cancellation)
                .await?
                .sent_actions(),
            1
        );
        let horizontal_wheel = InputEvent::Mouse {
            button: MouseButton::WheelRight,
            column: 0,
            row: 0,
            pressed: true,
            modifiers: Modifiers::empty(),
        };
        assert_eq!(
            input_adapter
                .dispatch(&mut target, &horizontal_wheel, &cancellation)
                .await?
                .sent_actions(),
            1
        );
        assert_eq!(
            input_adapter
                .dispatch(&mut target, &InputEvent::Focus(true), &cancellation)
                .await?
                .sent_actions(),
            1
        );
    }

    session.close().await?;
    finish_server(server).await
}

#[tokio::test]
async fn pre_cancelled_external_input_dispatch_sends_no_cdp_message() -> TestResult {
    let (endpoint, server) = start_mock(Vec::new()).await?;
    let mut session = CdpSession::connect(endpoint, cdp_limits()).await?;
    let input_adapter = adapter()?;
    let cancellation = Cancellation::new();
    cancellation.cancel();

    {
        let mut target = session.session(SESSION_ID)?;
        let error = input_adapter
            .dispatch(&mut target, &InputEvent::Focus(true), &cancellation)
            .await
            .err()
            .ok_or_else(|| io::Error::other("pre-cancelled dispatch unexpectedly succeeded"))?;
        assert!(matches!(error, ExternalInputError::Cancelled));
    }

    session.close().await?;
    finish_server(server).await
}
