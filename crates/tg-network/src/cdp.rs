//! A bounded Chrome DevTools Protocol client over a single WebSocket connection.
//!
//! The client deliberately owns no browser lifecycle. Callers provide a DevTools WebSocket
//! endpoint (for example the browser endpoint reported by Chrome) and can cancel an in-flight
//! operation by dropping its future or retire the transport explicitly with [`CdpSession::close`].

use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::time::{Instant, timeout_at};
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_with_config};

const DEFAULT_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_MAX_EVENTS: usize = 256;
const DEFAULT_MAX_PENDING_RESPONSES: usize = 256;
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_MAX_ACCESSIBILITY_NODES: usize = 4_096;
const DEFAULT_MAX_ACCESSIBILITY_DEPTH: usize = 128;
const HARD_MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
const HARD_MAX_EVENTS: usize = 4096;
const HARD_MAX_PENDING_RESPONSES: usize = 4096;
const HARD_MAX_OPERATION_TIMEOUT: Duration = Duration::from_secs(120);
const HARD_MAX_ACCESSIBILITY_NODES: usize = 100_000;
const HARD_MAX_ACCESSIBILITY_DEPTH: usize = 1_024;
const MAX_INPUT_COORDINATE: f64 = 10_000_000.0;
const MAX_MOUSE_DELTA: f64 = 1_000_000.0;
const MAX_DEVICE_DIMENSION: u32 = 100_000;
const MAX_DEVICE_SCALE: f64 = 100.0;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Hard bounds for one Chrome DevTools Protocol session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpLimits {
    /// Maximum UTF-8 JSON payload accepted in either direction.
    pub max_message_bytes: usize,
    /// Maximum unsolicited protocol events retained while callers await responses.
    pub max_events: usize,
    /// Maximum out-of-order responses retained for their matching request IDs.
    pub max_pending_responses: usize,
    /// Deadline shared by each connect, send, response, and event-wait operation.
    pub operation_timeout: Duration,
    /// Maximum typed nodes accepted from `Accessibility.getFullAXTree`.
    pub max_accessibility_nodes: usize,
    /// Maximum root-inclusive depth accepted from `Accessibility.getFullAXTree`.
    pub max_accessibility_depth: usize,
}

impl Default for CdpLimits {
    fn default() -> Self {
        Self {
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
            max_events: DEFAULT_MAX_EVENTS,
            max_pending_responses: DEFAULT_MAX_PENDING_RESPONSES,
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            max_accessibility_nodes: DEFAULT_MAX_ACCESSIBILITY_NODES,
            max_accessibility_depth: DEFAULT_MAX_ACCESSIBILITY_DEPTH,
        }
    }
}

impl CdpLimits {
    fn validate(&self) -> Result<(), CdpError> {
        if self.max_message_bytes == 0
            || self.max_message_bytes > HARD_MAX_MESSAGE_BYTES
            || self.max_events == 0
            || self.max_events > HARD_MAX_EVENTS
            || self.max_pending_responses == 0
            || self.max_pending_responses > HARD_MAX_PENDING_RESPONSES
            || self.operation_timeout.is_zero()
            || self.operation_timeout > HARD_MAX_OPERATION_TIMEOUT
            || self.max_accessibility_nodes == 0
            || self.max_accessibility_nodes > HARD_MAX_ACCESSIBILITY_NODES
            || self.max_accessibility_depth == 0
            || self.max_accessibility_depth > HARD_MAX_ACCESSIBILITY_DEPTH
        {
            return Err(CdpError::InvalidLimits);
        }
        Ok(())
    }
}

/// A successful response correlated by the monotonic CDP request ID.
#[derive(Debug, Clone, PartialEq)]
pub struct CdpReply {
    pub id: u64,
    pub session_id: Option<String>,
    pub result: Value,
}

/// An unsolicited CDP event retained until a caller consumes it.
#[derive(Debug, Clone, PartialEq)]
pub struct CdpEvent {
    pub method: String,
    pub session_id: Option<String>,
    pub params: Value,
}

/// Browser-provided protocol failure for a correlated request.
#[derive(Debug, Clone, PartialEq, Error)]
#[error("CDP protocol error for request {request_id}: {code} {message}")]
pub struct CdpProtocolError {
    pub request_id: u64,
    pub session_id: Option<String>,
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

/// The result returned by `Page.navigate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpNavigation {
    pub frame_id: String,
    pub loader_id: Option<String>,
    pub error_text: Option<String>,
}

/// The timestamp carried by `Page.loadEventFired`.
#[derive(Debug, Clone, PartialEq)]
pub struct CdpLoadEvent {
    pub timestamp: f64,
}

/// Base64 screenshot payload returned by `Page.captureScreenshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpScreenshot {
    pub data: String,
}

/// `Runtime.evaluate` result with any browser exception details preserved for callers.
#[derive(Debug, Clone, PartialEq)]
pub struct CdpEvaluation {
    pub result: Value,
    pub exception_details: Option<Value>,
}

/// Bounded metadata returned by Chrome DevTools `Browser.getVersion`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpBrowserVersion {
    pub protocol_version: String,
    pub product: String,
    pub revision: String,
    pub user_agent: String,
    pub js_version: String,
}

impl CdpBrowserVersion {
    fn from_response(result: &Value, max_message_bytes: usize) -> Result<Self, CdpError> {
        Ok(Self {
            protocol_version: required_bounded_response_string(
                result,
                "protocolVersion",
                max_message_bytes,
            )?,
            product: required_bounded_response_string(result, "product", max_message_bytes)?,
            revision: required_bounded_response_string(result, "revision", max_message_bytes)?,
            user_agent: required_bounded_response_string(result, "userAgent", max_message_bytes)?,
            js_version: required_bounded_response_string(result, "jsVersion", max_message_bytes)?,
        })
    }
}

/// A bounded, typed snapshot returned by `Accessibility.getFullAXTree`.
///
/// The tree retains only stable accessibility data needed by automation clients. CDP-specific
/// implementation fields such as backend DOM node IDs are deliberately excluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CdpAccessibilityTree {
    pub nodes: Vec<CdpAccessibilityNode>,
}

impl CdpAccessibilityTree {
    fn from_response(result: &Value, limits: &CdpLimits) -> Result<Self, CdpError> {
        let nodes = result
            .get("nodes")
            .and_then(Value::as_array)
            .ok_or(CdpError::InvalidResponse("accessibility nodes"))?;
        if nodes.is_empty() {
            return Err(CdpError::InvalidResponse("accessibility nodes"));
        }
        if nodes.len() > limits.max_accessibility_nodes {
            return Err(CdpError::AccessibilityNodeLimit {
                limit: limits.max_accessibility_nodes,
            });
        }

        let mut parsed = Vec::with_capacity(nodes.len());
        let mut node_indexes = BTreeMap::new();
        for node in nodes {
            let node = CdpAccessibilityNode::from_response(node, limits.max_message_bytes)?;
            if node_indexes.insert(node.id.clone(), parsed.len()).is_some() {
                return Err(CdpError::InvalidResponse("accessibility nodeId"));
            }
            parsed.push(node);
        }

        for node in &parsed {
            if let Some(parent_id) = &node.parent_id
                && (parent_id == &node.id || !node_indexes.contains_key(parent_id))
            {
                return Err(CdpError::InvalidResponse("accessibility parentId"));
            }

            let mut child_indexes = BTreeMap::new();
            for child_id in &node.child_ids {
                let Some(&child_index) = node_indexes.get(child_id) else {
                    return Err(CdpError::InvalidResponse("accessibility childIds"));
                };
                if child_indexes.insert(child_id, child_index).is_some()
                    || parsed[child_index].parent_id.as_deref() != Some(node.id.as_str())
                {
                    return Err(CdpError::InvalidResponse("accessibility childIds"));
                }
            }
        }

        let mut depths = vec![0_usize; parsed.len()];
        let mut traversal_marks = vec![0_usize; parsed.len()];
        for start in 0..parsed.len() {
            if depths[start] != 0 {
                continue;
            }
            let mark = start
                .checked_add(1)
                .ok_or(CdpError::InvalidResponse("accessibility tree cycle"))?;
            let mut current = start;
            let mut chain = Vec::new();
            let inherited_depth = loop {
                if depths[current] != 0 {
                    break depths[current];
                }
                if traversal_marks[current] == mark {
                    return Err(CdpError::InvalidResponse("accessibility tree cycle"));
                }
                traversal_marks[current] = mark;
                chain.push(current);
                let Some(parent_id) = parsed[current].parent_id.as_deref() else {
                    break 0;
                };
                current = *node_indexes
                    .get(parent_id)
                    .ok_or(CdpError::InvalidResponse("accessibility parentId"))?;
            };
            let mut depth = inherited_depth;
            while let Some(index) = chain.pop() {
                depth = depth
                    .checked_add(1)
                    .ok_or(CdpError::AccessibilityDepthLimit {
                        limit: limits.max_accessibility_depth,
                    })?;
                if depth > limits.max_accessibility_depth {
                    return Err(CdpError::AccessibilityDepthLimit {
                        limit: limits.max_accessibility_depth,
                    });
                }
                depths[index] = depth;
            }
        }

        Ok(Self { nodes: parsed })
    }
}

/// One typed node from a bounded Chrome accessibility tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CdpAccessibilityNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub child_ids: Vec<String>,
    pub role: String,
    pub name: Option<String>,
    pub value: Option<CdpAccessibilityValue>,
    pub states: Vec<CdpAccessibilityState>,
}

impl CdpAccessibilityNode {
    fn from_response(value: &Value, max_message_bytes: usize) -> Result<Self, CdpError> {
        let object = value
            .as_object()
            .ok_or(CdpError::InvalidResponse("accessibility node"))?;
        let id = required_accessibility_identifier(object, "nodeId", max_message_bytes)?;
        let parent_id = optional_accessibility_identifier(object, "parentId", max_message_bytes)?;
        let child_ids = match object.get("childIds") {
            None => Vec::new(),
            Some(Value::Array(child_ids)) => child_ids
                .iter()
                .map(|child_id| {
                    child_id
                        .as_str()
                        .ok_or(CdpError::InvalidResponse("accessibility childIds"))
                        .and_then(|child_id| {
                            bounded_accessibility_identifier(
                                child_id,
                                "accessibility childIds",
                                max_message_bytes,
                            )
                        })
                })
                .collect::<Result<Vec<_>, _>>()?,
            Some(_) => return Err(CdpError::InvalidResponse("accessibility childIds")),
        };
        let role =
            required_accessibility_value(object, "role", "accessibility role", max_message_bytes)?
                .into_required_string("accessibility role")?;
        let name =
            optional_accessibility_value(object, "name", "accessibility name", max_message_bytes)?
                .map(|name| name.into_string("accessibility name"))
                .transpose()?;
        let value = optional_accessibility_value(
            object,
            "value",
            "accessibility value",
            max_message_bytes,
        )?;
        let mut states = match object.get("properties") {
            None => Vec::new(),
            Some(Value::Array(properties)) => {
                let mut states = Vec::with_capacity(properties.len());
                for property in properties {
                    if let Some(state) =
                        CdpAccessibilityState::from_response(property, max_message_bytes)?
                    {
                        states.push(state);
                    }
                }
                states
            }
            Some(_) => return Err(CdpError::InvalidResponse("accessibility properties")),
        };
        states.sort_by(|left, right| left.name.cmp(&right.name));
        if states
            .windows(2)
            .any(|states| states[0].name == states[1].name)
        {
            return Err(CdpError::InvalidResponse("accessibility property name"));
        }

        Ok(Self {
            id,
            parent_id,
            child_ids,
            role,
            name,
            value,
            states,
        })
    }
}

/// One named accessibility state or property exposed by Chrome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CdpAccessibilityState {
    pub name: String,
    pub value: CdpAccessibilityValue,
}

impl CdpAccessibilityState {
    fn from_response(value: &Value, max_message_bytes: usize) -> Result<Option<Self>, CdpError> {
        let object = value
            .as_object()
            .ok_or(CdpError::InvalidResponse("accessibility property"))?;
        let name = required_accessibility_identifier(object, "name", max_message_bytes)?;
        let value = object
            .get("value")
            .ok_or(CdpError::InvalidResponse("accessibility property value"))?;
        Ok(
            CdpAccessibilityValue::from_property_response(value, max_message_bytes)?
                .map(|value| Self { name, value }),
        )
    }
}

/// A JSON-safe scalar extracted from a CDP `AXValue`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CdpAccessibilityValue {
    String(String),
    Boolean(bool),
    Number(String),
}

impl CdpAccessibilityValue {
    fn from_response(
        value: &Value,
        field: &'static str,
        max_message_bytes: usize,
    ) -> Result<Self, CdpError> {
        let object = value.as_object().ok_or(CdpError::InvalidResponse(field))?;
        let _type = required_accessibility_identifier(object, "type", max_message_bytes)
            .map_err(|_| CdpError::InvalidResponse(field))?;
        let value = object
            .get("value")
            .ok_or(CdpError::InvalidResponse(field))?;
        match value {
            Value::String(value) => {
                if value.len() > max_message_bytes || value.contains('\0') {
                    return Err(CdpError::InvalidResponse(field));
                }
                Ok(Self::String(value.clone()))
            }
            Value::Bool(value) => Ok(Self::Boolean(*value)),
            Value::Number(value) => Ok(Self::Number(value.to_string())),
            Value::Null | Value::Array(_) | Value::Object(_) => {
                Err(CdpError::InvalidResponse(field))
            }
        }
    }

    fn from_property_response(
        value: &Value,
        max_message_bytes: usize,
    ) -> Result<Option<Self>, CdpError> {
        let object = value
            .as_object()
            .ok_or(CdpError::InvalidResponse("accessibility property value"))?;
        let value_type = required_accessibility_identifier(object, "type", max_message_bytes)
            .map_err(|_| CdpError::InvalidResponse("accessibility property value"))?;
        let value = object
            .get("value")
            .ok_or(CdpError::InvalidResponse("accessibility property value"))?;
        match value {
            Value::String(value) => {
                if value.len() > max_message_bytes || value.contains('\0') {
                    return Err(CdpError::InvalidResponse("accessibility property value"));
                }
                Ok(Some(Self::String(value.clone())))
            }
            Value::Bool(value) => Ok(Some(Self::Boolean(*value))),
            Value::Number(value) => Ok(Some(Self::Number(value.to_string()))),
            Value::Array(_) | Value::Object(_) | Value::Null
                if matches!(
                    value_type.as_str(),
                    "idref" | "idrefList" | "node" | "nodeList" | "domRelation" | "valueUndefined"
                ) =>
            {
                Ok(None)
            }
            Value::Array(_) | Value::Object(_) | Value::Null => {
                Err(CdpError::InvalidResponse("accessibility property value"))
            }
        }
    }

    fn into_required_string(self, field: &'static str) -> Result<String, CdpError> {
        match self {
            Self::String(value) if !value.is_empty() => Ok(value),
            Self::String(_) | Self::Boolean(_) | Self::Number(_) => {
                Err(CdpError::InvalidResponse(field))
            }
        }
    }

    fn into_string(self, field: &'static str) -> Result<String, CdpError> {
        match self {
            Self::String(value) => Ok(value),
            Self::Boolean(_) | Self::Number(_) => Err(CdpError::InvalidResponse(field)),
        }
    }
}

/// Supported `Input.dispatchMouseEvent` event kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdpMouseEventKind {
    Move,
    Press,
    Release,
    Wheel,
}

impl CdpMouseEventKind {
    const fn protocol_name(self) -> &'static str {
        match self {
            Self::Move => "mouseMoved",
            Self::Press => "mousePressed",
            Self::Release => "mouseReleased",
            Self::Wheel => "mouseWheel",
        }
    }
}

/// Mouse buttons accepted by `Input.dispatchMouseEvent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CdpMouseButton {
    None,
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

impl CdpMouseButton {
    const fn protocol_name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Left => "left",
            Self::Middle => "middle",
            Self::Right => "right",
            Self::Back => "back",
            Self::Forward => "forward",
        }
    }
}

/// Bounded input for `Input.dispatchMouseEvent`.
#[derive(Debug, Clone, PartialEq)]
pub struct CdpMouseEvent {
    pub kind: CdpMouseEventKind,
    pub x: f64,
    pub y: f64,
    pub button: CdpMouseButton,
    pub click_count: u8,
    pub delta_x: f64,
    pub delta_y: f64,
}

impl CdpMouseEvent {
    #[must_use]
    pub const fn click_at(x: f64, y: f64, pressed: bool) -> Self {
        Self {
            kind: if pressed {
                CdpMouseEventKind::Press
            } else {
                CdpMouseEventKind::Release
            },
            x,
            y,
            button: CdpMouseButton::Left,
            click_count: 1,
            delta_x: 0.0,
            delta_y: 0.0,
        }
    }

    fn validate(&self) -> Result<(), CdpError> {
        if !self.x.is_finite()
            || !self.y.is_finite()
            || !self.delta_x.is_finite()
            || !self.delta_y.is_finite()
            || self.x.abs() > MAX_INPUT_COORDINATE
            || self.y.abs() > MAX_INPUT_COORDINATE
            || self.delta_x.abs() > MAX_MOUSE_DELTA
            || self.delta_y.abs() > MAX_MOUSE_DELTA
        {
            return Err(CdpError::InvalidInput);
        }
        Ok(())
    }
}

/// Errors returned by a bounded CDP WebSocket session.
#[derive(Debug, Error)]
pub enum CdpError {
    #[error("CDP limits are invalid or exceed the hard bounds")]
    InvalidLimits,
    #[error("CDP endpoint must be a bounded ws or wss URL without credentials")]
    InvalidEndpoint,
    #[error("CDP operation timed out")]
    Timeout,
    #[error("CDP WebSocket connection is closed")]
    Closed,
    #[error("CDP WebSocket transport failed: {0}")]
    WebSocket(String),
    #[error("CDP WebSocket message exceeded its byte limit")]
    MessageTooLarge,
    #[error("CDP does not accept binary WebSocket messages")]
    BinaryMessage,
    #[error("CDP JSON message was malformed")]
    MalformedMessage,
    #[error("CDP event queue reached its limit of {limit}")]
    EventLimit { limit: usize },
    #[error("CDP pending response queue reached its limit of {limit}")]
    PendingResponseLimit { limit: usize },
    #[error("CDP request ID space is exhausted")]
    RequestIdExhausted,
    #[error("CDP method is malformed or too large")]
    InvalidMethod,
    #[error("CDP session ID is malformed or too large")]
    InvalidSessionId,
    #[error("CDP helper input is malformed or exceeds a byte bound")]
    InvalidInput,
    #[error("CDP response did not contain the required {0} field")]
    InvalidResponse(&'static str),
    #[error("CDP accessibility tree exceeded its node limit of {limit}")]
    AccessibilityNodeLimit { limit: usize },
    #[error("CDP accessibility tree exceeded its depth limit of {limit}")]
    AccessibilityDepthLimit { limit: usize },
    #[error("CDP input queue is backpressured")]
    Backpressure,
    #[error(transparent)]
    Protocol(Box<CdpProtocolError>),
}

impl From<CdpProtocolError> for CdpError {
    fn from(error: CdpProtocolError) -> Self {
        Self::Protocol(Box::new(error))
    }
}

enum Incoming {
    Reply(CdpReply),
    Protocol(CdpProtocolError),
    Event(CdpEvent),
}

impl Incoming {
    fn into_response(self) -> Option<(u64, Result<CdpReply, CdpProtocolError>)> {
        match self {
            Self::Reply(reply) => Some((reply.id, Ok(reply))),
            Self::Protocol(error) => Some((error.request_id, Err(error))),
            Self::Event(_) => None,
        }
    }
}

/// A single bounded DevTools WebSocket connection.
///
/// No background task is spawned. Dropping an in-flight operation cancels its socket future;
/// callers can reconnect explicitly after a close, crash, timeout, or protocol violation.
pub struct CdpSession {
    endpoint: String,
    limits: CdpLimits,
    socket: Option<Socket>,
    next_request_id: u64,
    events: VecDeque<CdpEvent>,
    pending_responses: BTreeMap<u64, Result<CdpReply, CdpProtocolError>>,
}

impl fmt::Debug for CdpSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CdpSession")
            .field("endpoint", &self.endpoint)
            .field("limits", &self.limits)
            .field("connected", &self.socket.is_some())
            .field("next_request_id", &self.next_request_id)
            .field("queued_events", &self.events.len())
            .field("pending_responses", &self.pending_responses.len())
            .finish()
    }
}

impl CdpSession {
    /// Opens one WebSocket connection to a Chrome DevTools endpoint.
    pub async fn connect(endpoint: impl AsRef<str>, limits: CdpLimits) -> Result<Self, CdpError> {
        limits.validate()?;
        let endpoint = endpoint.as_ref().to_owned();
        validate_endpoint(&endpoint, &limits)?;
        let socket = open_socket(&endpoint, &limits).await?;
        Ok(Self {
            endpoint,
            limits,
            socket: Some(socket),
            next_request_id: 1,
            events: VecDeque::new(),
            pending_responses: BTreeMap::new(),
        })
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    pub fn limits(&self) -> &CdpLimits {
        &self.limits
    }

    #[must_use]
    pub fn is_connected(&self) -> bool {
        self.socket.is_some()
    }

    #[must_use]
    pub fn queued_event_count(&self) -> usize {
        self.events.len()
    }

    /// Returns bounded version metadata from the loopback DevTools peer.
    pub async fn browser_version(&mut self) -> Result<CdpBrowserVersion, CdpError> {
        let reply = self.send("Browser.getVersion", Value::Null).await?;
        CdpBrowserVersion::from_response(&reply.result, self.limits.max_message_bytes)
    }

    /// Enables Chrome's Accessibility domain for the browser-level session.
    ///
    /// This helper inherits the session's bounded deadline and close behavior. Dropping the
    /// returned future cancels the in-flight socket operation without spawning background work.
    pub async fn accessibility_enable(&mut self) -> Result<(), CdpError> {
        self.accessibility_enable_for(None).await
    }

    /// Returns a bounded, typed browser-level accessibility tree after `accessibility_enable`.
    ///
    /// This helper inherits the session's bounded deadline and close behavior. Dropping the
    /// returned future cancels the in-flight socket operation without spawning background work.
    pub async fn accessibility_tree(&mut self) -> Result<CdpAccessibilityTree, CdpError> {
        self.accessibility_tree_for(None).await
    }

    /// Drops stale transport state and opens a fresh connection without reusing request IDs.
    pub async fn reconnect(&mut self) -> Result<(), CdpError> {
        self.socket = None;
        self.events.clear();
        self.pending_responses.clear();
        self.socket = Some(open_socket(&self.endpoint, &self.limits).await?);
        Ok(())
    }

    /// Closes the current WebSocket transport. Reconnect to reuse this session object.
    pub async fn close(&mut self) -> Result<(), CdpError> {
        let Some(mut socket) = self.socket.take() else {
            return Ok(());
        };
        self.events.clear();
        self.pending_responses.clear();
        let deadline = self.deadline();
        timeout_at(deadline, socket.send(Message::Close(None)))
            .await
            .map_err(|_| CdpError::Timeout)?
            .map_err(map_socket_error)
    }

    /// Sends a top-level CDP command and waits for the response with its matching request ID.
    pub async fn send(&mut self, method: &str, params: Value) -> Result<CdpReply, CdpError> {
        self.send_for_session(None, method, params).await
    }

    /// Sends a CDP command with a top-level flattened `sessionId` and correlates only by ID.
    pub async fn send_in_session(
        &mut self,
        session_id: &str,
        method: &str,
        params: Value,
    ) -> Result<CdpReply, CdpError> {
        self.send_for_session(Some(session_id), method, params)
            .await
    }

    /// Returns the oldest unsolicited event without waiting for more transport input.
    pub fn next_event(&mut self) -> Option<CdpEvent> {
        self.events.pop_front()
    }

    /// Waits for one top-level event while retaining all nonmatching events and responses.
    pub async fn wait_for_event(&mut self, method: &str) -> Result<CdpEvent, CdpError> {
        self.wait_for_event_in_session(None, method).await
    }

    /// Enables the Page domain on the browser-level session.
    pub async fn page_enable(&mut self) -> Result<(), CdpError> {
        self.page_enable_for(None).await
    }

    /// Returns the positive root node ID reported by `DOM.getDocument`.
    pub async fn dom_get_document(&mut self) -> Result<u64, CdpError> {
        self.dom_get_document_for(None).await
    }

    /// Returns the bounded serialized outer HTML for one positive DOM node ID.
    pub async fn dom_get_outer_html(&mut self, node_id: u64) -> Result<String, CdpError> {
        self.dom_get_outer_html_for(None, node_id).await
    }

    /// Navigates the browser-level page session to a caller-provided URL.
    pub async fn navigate(&mut self, url: &str) -> Result<CdpNavigation, CdpError> {
        self.navigate_for(None, url).await
    }

    /// Waits for a browser-level `Page.loadEventFired` event.
    pub async fn wait_for_load(&mut self) -> Result<CdpLoadEvent, CdpError> {
        self.wait_for_load_in_session(None).await
    }

    /// Captures a browser-level screenshot using Chrome's default screenshot settings.
    pub async fn capture_screenshot(&mut self) -> Result<CdpScreenshot, CdpError> {
        self.capture_screenshot_for(None).await
    }

    /// Evaluates JavaScript in the browser-level execution context.
    pub async fn runtime_evaluate(
        &mut self,
        expression: &str,
        return_by_value: bool,
    ) -> Result<CdpEvaluation, CdpError> {
        self.runtime_evaluate_for(None, expression, return_by_value)
            .await
    }

    /// Applies bounded emulated device metrics to the browser-level session.
    pub async fn set_device_metrics(
        &mut self,
        width: u32,
        height: u32,
        scale: f64,
    ) -> Result<(), CdpError> {
        self.set_device_metrics_for(None, width, height, scale)
            .await
    }

    /// Reloads the browser-level page session.
    pub async fn reload(&mut self) -> Result<(), CdpError> {
        self.reload_for(None).await
    }

    /// Sends text through the browser-level Input domain.
    pub async fn insert_text(&mut self, text: &str) -> Result<(), CdpError> {
        self.insert_text_for(None, text).await
    }

    /// Dispatches a browser-level keyboard key-down event.
    pub async fn key_down(&mut self, key: &str) -> Result<(), CdpError> {
        self.key_event_for(None, "keyDown", key).await
    }

    /// Dispatches a browser-level keyboard key-up event.
    pub async fn key_up(&mut self, key: &str) -> Result<(), CdpError> {
        self.key_event_for(None, "keyUp", key).await
    }

    /// Dispatches a browser-level mouse event, including bounded wheel deltas.
    pub async fn dispatch_mouse_event(&mut self, event: CdpMouseEvent) -> Result<(), CdpError> {
        self.dispatch_mouse_event_for(None, event).await
    }

    /// Sends a left-button press and release at the supplied browser-level coordinates.
    pub async fn mouse_click(&mut self, x: f64, y: f64) -> Result<(), CdpError> {
        self.mouse_click_for(None, x, y).await
    }

    /// Moves the browser-level history cursor by a bounded caller-provided delta.
    pub async fn navigate_history_delta(&mut self, delta: i32) -> Result<(), CdpError> {
        self.navigate_history_delta_for(None, delta).await
    }

    /// Returns a borrowed client scoped to an already-attached flattened target session.
    pub fn session(
        &mut self,
        session_id: impl Into<String>,
    ) -> Result<CdpTargetSession<'_>, CdpError> {
        let session_id = session_id.into();
        validate_session_id(&session_id, self.limits.max_message_bytes)?;
        Ok(CdpTargetSession {
            connection: self,
            session_id,
        })
    }

    /// Attaches to the preferred first page target without requiring a `/json/list` request.
    pub async fn attach_first_page(&mut self) -> Result<CdpTargetSession<'_>, CdpError> {
        let targets = self
            .send("Target.getTargets", Value::Object(Map::new()))
            .await?;
        let target_id = select_first_page_target(&targets.result)?;
        let attached = self
            .send(
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": true }),
            )
            .await?;
        let session_id = required_string(&attached.result, "sessionId")?.to_owned();
        validate_session_id(&session_id, self.limits.max_message_bytes)?;
        Ok(CdpTargetSession {
            connection: self,
            session_id,
        })
    }

    async fn send_for_session(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<CdpReply, CdpError> {
        validate_method(method, self.limits.max_message_bytes)?;
        if let Some(session_id) = session_id {
            validate_session_id(session_id, self.limits.max_message_bytes)?;
        }
        let request_id = self.allocate_request_id()?;
        let mut object = Map::new();
        object.insert("id".to_owned(), Value::from(request_id));
        object.insert("method".to_owned(), Value::from(method));
        if !params.is_null() {
            object.insert("params".to_owned(), params);
        }
        if let Some(session_id) = session_id {
            object.insert("sessionId".to_owned(), Value::from(session_id));
        }
        let message =
            serde_json::to_string(&Value::Object(object)).map_err(|_| CdpError::InvalidInput)?;
        if message.len() > self.limits.max_message_bytes {
            return Err(CdpError::MessageTooLarge);
        }
        let deadline = self.deadline();
        self.send_text(message, deadline).await?;
        self.wait_for_response(request_id, deadline).await
    }

    fn allocate_request_id(&mut self) -> Result<u64, CdpError> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or(CdpError::RequestIdExhausted)?;
        Ok(request_id)
    }

    fn deadline(&self) -> Instant {
        Instant::now() + self.limits.operation_timeout
    }

    async fn send_text(&mut self, text: String, deadline: Instant) -> Result<(), CdpError> {
        let result = {
            let socket = self.socket.as_mut().ok_or(CdpError::Closed)?;
            timeout_at(deadline, socket.send(Message::Text(text.into()))).await
        };
        match result {
            Err(_) => Err(CdpError::Timeout),
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                let error = map_socket_error(error);
                if error_is_terminal(&error) {
                    self.invalidate_transport();
                }
                Err(error)
            }
        }
    }

    async fn flush(&mut self, deadline: Instant) -> Result<(), CdpError> {
        let result = {
            let socket = self.socket.as_mut().ok_or(CdpError::Closed)?;
            timeout_at(deadline, socket.flush()).await
        };
        match result {
            Err(_) => Err(CdpError::Timeout),
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) => {
                let error = map_socket_error(error);
                if error_is_terminal(&error) {
                    self.invalidate_transport();
                }
                Err(error)
            }
        }
    }

    async fn wait_for_response(
        &mut self,
        request_id: u64,
        deadline: Instant,
    ) -> Result<CdpReply, CdpError> {
        if let Some(response) = self.pending_responses.remove(&request_id) {
            return response.map_err(CdpError::from);
        }
        loop {
            match self.receive_incoming(deadline).await? {
                Incoming::Event(event) => self.push_event(event)?,
                incoming @ (Incoming::Reply(_) | Incoming::Protocol(_)) => {
                    let (received_id, response) =
                        incoming.into_response().ok_or(CdpError::MalformedMessage)?;
                    if received_id == request_id {
                        return response.map_err(CdpError::from);
                    }
                    self.push_response(received_id, response)?;
                }
            }
        }
    }

    async fn wait_for_event_in_session(
        &mut self,
        session_id: Option<&str>,
        method: &str,
    ) -> Result<CdpEvent, CdpError> {
        validate_method(method, self.limits.max_message_bytes)?;
        if let Some(session_id) = session_id {
            validate_session_id(session_id, self.limits.max_message_bytes)?;
        }
        if let Some(index) = self
            .events
            .iter()
            .position(|event| event.method == method && event.session_id.as_deref() == session_id)
        {
            return self.events.remove(index).ok_or(CdpError::MalformedMessage);
        }
        let deadline = self.deadline();
        loop {
            match self.receive_incoming(deadline).await? {
                Incoming::Event(event)
                    if event.method == method && event.session_id.as_deref() == session_id =>
                {
                    return Ok(event);
                }
                Incoming::Event(event) => self.push_event(event)?,
                incoming @ (Incoming::Reply(_) | Incoming::Protocol(_)) => {
                    let (received_id, response) =
                        incoming.into_response().ok_or(CdpError::MalformedMessage)?;
                    self.push_response(received_id, response)?;
                }
            }
        }
    }

    async fn receive_incoming(&mut self, deadline: Instant) -> Result<Incoming, CdpError> {
        loop {
            let message = self.receive_message(deadline).await?;
            match message {
                Message::Text(text) => match self.parse_incoming(text.to_string()) {
                    Ok(incoming) => return Ok(incoming),
                    Err(error) => {
                        self.invalidate_transport();
                        return Err(error);
                    }
                },
                Message::Binary(_) => {
                    self.invalidate_transport();
                    return Err(CdpError::BinaryMessage);
                }
                Message::Ping(_) | Message::Pong(_) => self.flush(deadline).await?,
                Message::Close(_) => {
                    self.invalidate_transport();
                    return Err(CdpError::Closed);
                }
                Message::Frame(_) => {
                    self.invalidate_transport();
                    return Err(CdpError::MalformedMessage);
                }
            }
        }
    }

    async fn receive_message(&mut self, deadline: Instant) -> Result<Message, CdpError> {
        let result = {
            let socket = self.socket.as_mut().ok_or(CdpError::Closed)?;
            timeout_at(deadline, socket.next()).await
        };
        match result {
            Err(_) => Err(CdpError::Timeout),
            Ok(None) => {
                self.invalidate_transport();
                Err(CdpError::Closed)
            }
            Ok(Some(Ok(message))) => Ok(message),
            Ok(Some(Err(error))) => {
                let error = map_socket_error(error);
                if error_is_terminal(&error) {
                    self.invalidate_transport();
                }
                Err(error)
            }
        }
    }

    fn parse_incoming(&self, text: String) -> Result<Incoming, CdpError> {
        if text.len() > self.limits.max_message_bytes {
            return Err(CdpError::MessageTooLarge);
        }
        let value: Value = serde_json::from_str(&text).map_err(|_| CdpError::MalformedMessage)?;
        let object = value.as_object().ok_or(CdpError::MalformedMessage)?;
        let session_id = optional_string(object, "sessionId", self.limits.max_message_bytes)?;
        if let Some(id) = object.get("id") {
            if object.contains_key("method") {
                return Err(CdpError::MalformedMessage);
            }
            let id = id
                .as_u64()
                .filter(|id| *id > 0)
                .ok_or(CdpError::MalformedMessage)?;
            let result = object.get("result");
            let error = object.get("error");
            return match (result, error) {
                (Some(result), None) => Ok(Incoming::Reply(CdpReply {
                    id,
                    session_id,
                    result: result.clone(),
                })),
                (None, Some(error)) => Ok(Incoming::Protocol(parse_protocol_error(
                    id, session_id, error,
                )?)),
                _ => Err(CdpError::MalformedMessage),
            };
        }
        let method = object
            .get("method")
            .and_then(Value::as_str)
            .ok_or(CdpError::MalformedMessage)?;
        validate_method(method, self.limits.max_message_bytes)?;
        Ok(Incoming::Event(CdpEvent {
            method: method.to_owned(),
            session_id,
            params: object.get("params").cloned().unwrap_or(Value::Null),
        }))
    }

    fn push_event(&mut self, event: CdpEvent) -> Result<(), CdpError> {
        if self.events.len() >= self.limits.max_events {
            return Err(CdpError::EventLimit {
                limit: self.limits.max_events,
            });
        }
        self.events.push_back(event);
        Ok(())
    }

    fn push_response(
        &mut self,
        request_id: u64,
        response: Result<CdpReply, CdpProtocolError>,
    ) -> Result<(), CdpError> {
        if request_id >= self.next_request_id {
            return Err(CdpError::MalformedMessage);
        }
        if self.pending_responses.contains_key(&request_id)
            || self.pending_responses.len() >= self.limits.max_pending_responses
        {
            return Err(CdpError::PendingResponseLimit {
                limit: self.limits.max_pending_responses,
            });
        }
        self.pending_responses.insert(request_id, response);
        Ok(())
    }

    fn invalidate_transport(&mut self) {
        self.socket = None;
        self.pending_responses.clear();
    }

    async fn page_enable_for(&mut self, session_id: Option<&str>) -> Result<(), CdpError> {
        self.send_optional_session(session_id, "Page.enable", Value::Object(Map::new()))
            .await
            .map(|_| ())
    }

    async fn dom_get_document_for(&mut self, session_id: Option<&str>) -> Result<u64, CdpError> {
        let reply = self
            .send_optional_session(
                session_id,
                "DOM.getDocument",
                json!({ "depth": 0, "pierce": false }),
            )
            .await?;
        required_dom_node_id(&reply.result)
    }

    async fn dom_get_outer_html_for(
        &mut self,
        session_id: Option<&str>,
        node_id: u64,
    ) -> Result<String, CdpError> {
        let node_id = checked_dom_node_id(node_id)?;
        let reply = self
            .send_optional_session(session_id, "DOM.getOuterHTML", json!({ "nodeId": node_id }))
            .await?;
        required_bounded_document_string(&reply.result, "outerHTML", self.limits.max_message_bytes)
    }

    async fn accessibility_enable_for(&mut self, session_id: Option<&str>) -> Result<(), CdpError> {
        self.send_optional_session(
            session_id,
            "Accessibility.enable",
            Value::Object(Map::new()),
        )
        .await
        .map(|_| ())
    }

    async fn accessibility_tree_for(
        &mut self,
        session_id: Option<&str>,
    ) -> Result<CdpAccessibilityTree, CdpError> {
        let reply = self
            .send_optional_session(
                session_id,
                "Accessibility.getFullAXTree",
                Value::Object(Map::new()),
            )
            .await?;
        CdpAccessibilityTree::from_response(&reply.result, &self.limits)
    }

    async fn navigate_for(
        &mut self,
        session_id: Option<&str>,
        url: &str,
    ) -> Result<CdpNavigation, CdpError> {
        validate_text(url, self.limits.max_message_bytes)?;
        if url.is_empty() {
            return Err(CdpError::InvalidInput);
        }
        let reply = self
            .send_optional_session(session_id, "Page.navigate", json!({ "url": url }))
            .await?;
        Ok(CdpNavigation {
            frame_id: required_string(&reply.result, "frameId")?.to_owned(),
            loader_id: optional_response_string(&reply.result, "loaderId")?,
            error_text: optional_response_string(&reply.result, "errorText")?,
        })
    }

    async fn wait_for_load_in_session(
        &mut self,
        session_id: Option<&str>,
    ) -> Result<CdpLoadEvent, CdpError> {
        let event = self
            .wait_for_event_in_session(session_id, "Page.loadEventFired")
            .await?;
        let timestamp = event
            .params
            .get("timestamp")
            .and_then(Value::as_f64)
            .filter(|timestamp| timestamp.is_finite())
            .ok_or(CdpError::InvalidResponse("timestamp"))?;
        Ok(CdpLoadEvent { timestamp })
    }

    async fn capture_screenshot_for(
        &mut self,
        session_id: Option<&str>,
    ) -> Result<CdpScreenshot, CdpError> {
        let reply = self
            .send_optional_session(
                session_id,
                "Page.captureScreenshot",
                Value::Object(Map::new()),
            )
            .await?;
        let data = required_string(&reply.result, "data")?;
        validate_text(data, self.limits.max_message_bytes)?;
        Ok(CdpScreenshot {
            data: data.to_owned(),
        })
    }

    async fn runtime_evaluate_for(
        &mut self,
        session_id: Option<&str>,
        expression: &str,
        return_by_value: bool,
    ) -> Result<CdpEvaluation, CdpError> {
        validate_text(expression, self.limits.max_message_bytes)?;
        if expression.is_empty() {
            return Err(CdpError::InvalidInput);
        }
        let reply = self
            .send_optional_session(
                session_id,
                "Runtime.evaluate",
                json!({ "expression": expression, "returnByValue": return_by_value }),
            )
            .await?;
        Ok(CdpEvaluation {
            result: reply
                .result
                .get("result")
                .cloned()
                .ok_or(CdpError::InvalidResponse("result"))?,
            exception_details: reply.result.get("exceptionDetails").cloned(),
        })
    }

    async fn set_device_metrics_for(
        &mut self,
        session_id: Option<&str>,
        width: u32,
        height: u32,
        scale: f64,
    ) -> Result<(), CdpError> {
        if width == 0
            || height == 0
            || width > MAX_DEVICE_DIMENSION
            || height > MAX_DEVICE_DIMENSION
            || !scale.is_finite()
            || scale <= 0.0
            || scale > MAX_DEVICE_SCALE
        {
            return Err(CdpError::InvalidInput);
        }
        self.send_optional_session(
            session_id,
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width": width,
                "height": height,
                "deviceScaleFactor": scale,
                "mobile": false,
            }),
        )
        .await
        .map(|_| ())
    }

    async fn reload_for(&mut self, session_id: Option<&str>) -> Result<(), CdpError> {
        self.send_optional_session(session_id, "Page.reload", Value::Object(Map::new()))
            .await
            .map(|_| ())
    }

    async fn insert_text_for(
        &mut self,
        session_id: Option<&str>,
        text: &str,
    ) -> Result<(), CdpError> {
        validate_text(text, self.limits.max_message_bytes)?;
        self.send_optional_session(session_id, "Input.insertText", json!({ "text": text }))
            .await
            .map(|_| ())
    }

    async fn key_event_for(
        &mut self,
        session_id: Option<&str>,
        event_type: &'static str,
        key: &str,
    ) -> Result<(), CdpError> {
        validate_text(key, self.limits.max_message_bytes)?;
        if key.is_empty() {
            return Err(CdpError::InvalidInput);
        }
        self.send_optional_session(
            session_id,
            "Input.dispatchKeyEvent",
            json!({ "type": event_type, "key": key }),
        )
        .await
        .map(|_| ())
    }

    async fn dispatch_mouse_event_for(
        &mut self,
        session_id: Option<&str>,
        event: CdpMouseEvent,
    ) -> Result<(), CdpError> {
        event.validate()?;
        self.send_optional_session(
            session_id,
            "Input.dispatchMouseEvent",
            json!({
                "type": event.kind.protocol_name(),
                "x": event.x,
                "y": event.y,
                "button": event.button.protocol_name(),
                "clickCount": event.click_count,
                "deltaX": event.delta_x,
                "deltaY": event.delta_y,
            }),
        )
        .await
        .map(|_| ())
    }

    async fn mouse_click_for(
        &mut self,
        session_id: Option<&str>,
        x: f64,
        y: f64,
    ) -> Result<(), CdpError> {
        self.dispatch_mouse_event_for(session_id, CdpMouseEvent::click_at(x, y, true))
            .await?;
        self.dispatch_mouse_event_for(session_id, CdpMouseEvent::click_at(x, y, false))
            .await
    }

    async fn navigate_history_delta_for(
        &mut self,
        session_id: Option<&str>,
        delta: i32,
    ) -> Result<(), CdpError> {
        if delta == 0 {
            return Err(CdpError::InvalidInput);
        }
        let history = self
            .send_optional_session(
                session_id,
                "Page.getNavigationHistory",
                Value::Object(Map::new()),
            )
            .await?;
        let current_index = history
            .result
            .get("currentIndex")
            .and_then(Value::as_i64)
            .ok_or(CdpError::InvalidResponse("currentIndex"))?;
        let entries = history
            .result
            .get("entries")
            .and_then(Value::as_array)
            .ok_or(CdpError::InvalidResponse("entries"))?;
        let target_index = current_index
            .checked_add(i64::from(delta))
            .ok_or(CdpError::InvalidInput)?;
        let entry = usize::try_from(target_index)
            .ok()
            .and_then(|index| entries.get(index))
            .ok_or(CdpError::InvalidInput)?;
        let entry_id = entry
            .get("id")
            .and_then(Value::as_i64)
            .ok_or(CdpError::InvalidResponse("id"))?;
        self.send_optional_session(
            session_id,
            "Page.navigateToHistoryEntry",
            json!({ "entryId": entry_id }),
        )
        .await
        .map(|_| ())
    }

    async fn send_optional_session(
        &mut self,
        session_id: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<CdpReply, CdpError> {
        match session_id {
            Some(session_id) => self.send_in_session(session_id, method, params).await,
            None => self.send(method, params).await,
        }
    }
}

/// A borrowed client that automatically applies one flattened DevTools `sessionId`.
pub struct CdpTargetSession<'connection> {
    connection: &'connection mut CdpSession,
    session_id: String,
}

impl fmt::Debug for CdpTargetSession<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CdpTargetSession")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl CdpTargetSession<'_> {
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Sends any CDP command while preserving this target's top-level `sessionId`.
    pub async fn send(&mut self, method: &str, params: Value) -> Result<CdpReply, CdpError> {
        self.connection
            .send_in_session(&self.session_id, method, params)
            .await
    }

    /// Waits for an event emitted by this flattened target session.
    pub async fn wait_for_event(&mut self, method: &str) -> Result<CdpEvent, CdpError> {
        self.connection
            .wait_for_event_in_session(Some(&self.session_id), method)
            .await
    }

    pub async fn page_enable(&mut self) -> Result<(), CdpError> {
        self.connection
            .page_enable_for(Some(&self.session_id))
            .await
    }

    /// Returns the positive root node ID reported by `DOM.getDocument` for this target.
    pub async fn dom_get_document(&mut self) -> Result<u64, CdpError> {
        self.connection
            .dom_get_document_for(Some(&self.session_id))
            .await
    }

    /// Returns the bounded serialized outer HTML for one positive node ID in this target.
    pub async fn dom_get_outer_html(&mut self, node_id: u64) -> Result<String, CdpError> {
        self.connection
            .dom_get_outer_html_for(Some(&self.session_id), node_id)
            .await
    }

    /// Enables Chrome's Accessibility domain for this flattened target session.
    pub async fn accessibility_enable(&mut self) -> Result<(), CdpError> {
        self.connection
            .accessibility_enable_for(Some(&self.session_id))
            .await
    }

    /// Returns the bounded, typed accessibility tree for this flattened target session.
    pub async fn accessibility_tree(&mut self) -> Result<CdpAccessibilityTree, CdpError> {
        self.connection
            .accessibility_tree_for(Some(&self.session_id))
            .await
    }

    pub async fn navigate(&mut self, url: &str) -> Result<CdpNavigation, CdpError> {
        self.connection
            .navigate_for(Some(&self.session_id), url)
            .await
    }

    pub async fn wait_for_load(&mut self) -> Result<CdpLoadEvent, CdpError> {
        self.connection
            .wait_for_load_in_session(Some(&self.session_id))
            .await
    }

    pub async fn capture_screenshot(&mut self) -> Result<CdpScreenshot, CdpError> {
        self.connection
            .capture_screenshot_for(Some(&self.session_id))
            .await
    }

    pub async fn runtime_evaluate(
        &mut self,
        expression: &str,
        return_by_value: bool,
    ) -> Result<CdpEvaluation, CdpError> {
        self.connection
            .runtime_evaluate_for(Some(&self.session_id), expression, return_by_value)
            .await
    }

    pub async fn set_device_metrics(
        &mut self,
        width: u32,
        height: u32,
        scale: f64,
    ) -> Result<(), CdpError> {
        self.connection
            .set_device_metrics_for(Some(&self.session_id), width, height, scale)
            .await
    }

    pub async fn reload(&mut self) -> Result<(), CdpError> {
        self.connection.reload_for(Some(&self.session_id)).await
    }

    pub async fn insert_text(&mut self, text: &str) -> Result<(), CdpError> {
        self.connection
            .insert_text_for(Some(&self.session_id), text)
            .await
    }

    pub async fn key_down(&mut self, key: &str) -> Result<(), CdpError> {
        self.connection
            .key_event_for(Some(&self.session_id), "keyDown", key)
            .await
    }

    pub async fn key_up(&mut self, key: &str) -> Result<(), CdpError> {
        self.connection
            .key_event_for(Some(&self.session_id), "keyUp", key)
            .await
    }

    pub async fn dispatch_mouse_event(&mut self, event: CdpMouseEvent) -> Result<(), CdpError> {
        self.connection
            .dispatch_mouse_event_for(Some(&self.session_id), event)
            .await
    }

    pub async fn mouse_click(&mut self, x: f64, y: f64) -> Result<(), CdpError> {
        self.connection
            .mouse_click_for(Some(&self.session_id), x, y)
            .await
    }

    pub async fn navigate_history_delta(&mut self, delta: i32) -> Result<(), CdpError> {
        self.connection
            .navigate_history_delta_for(Some(&self.session_id), delta)
            .await
    }
}

async fn open_socket(endpoint: &str, limits: &CdpLimits) -> Result<Socket, CdpError> {
    let config = WebSocketConfig::default()
        .max_message_size(Some(limits.max_message_bytes))
        .max_frame_size(Some(limits.max_message_bytes));
    let connection = timeout_at(
        Instant::now() + limits.operation_timeout,
        connect_async_with_config(endpoint.to_owned(), Some(config), true),
    )
    .await
    .map_err(|_| CdpError::Timeout)?
    .map_err(map_socket_error)?;
    Ok(connection.0)
}

fn validate_endpoint(endpoint: &str, limits: &CdpLimits) -> Result<(), CdpError> {
    if endpoint.is_empty() || endpoint.len() > limits.max_message_bytes || endpoint.contains('\0') {
        return Err(CdpError::InvalidEndpoint);
    }
    let request = endpoint
        .into_client_request()
        .map_err(|_| CdpError::InvalidEndpoint)?;
    let uri = request.uri();
    let Some(authority) = uri.authority() else {
        return Err(CdpError::InvalidEndpoint);
    };
    let host = authority.host();
    if !matches!(uri.scheme_str(), Some("ws" | "wss"))
        || authority.as_str().contains('@')
        || !(host == "127.0.0.1" || host == "[::1]" || host.eq_ignore_ascii_case("localhost"))
    {
        return Err(CdpError::InvalidEndpoint);
    }
    Ok(())
}

fn validate_method(method: &str, max_bytes: usize) -> Result<(), CdpError> {
    if method.is_empty()
        || method.len() > max_bytes
        || !method
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'$'))
    {
        return Err(CdpError::InvalidMethod);
    }
    Ok(())
}

fn validate_session_id(session_id: &str, max_bytes: usize) -> Result<(), CdpError> {
    if session_id.is_empty()
        || session_id.len() > max_bytes
        || session_id.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(CdpError::InvalidSessionId);
    }
    Ok(())
}

fn validate_text(value: &str, max_bytes: usize) -> Result<(), CdpError> {
    if value.len() > max_bytes || value.contains('\0') {
        return Err(CdpError::InvalidInput);
    }
    Ok(())
}

fn optional_string(
    object: &Map<String, Value>,
    name: &'static str,
    max_bytes: usize,
) -> Result<Option<String>, CdpError> {
    let Some(value) = object.get(name) else {
        return Ok(None);
    };
    let value = value.as_str().ok_or(CdpError::MalformedMessage)?;
    validate_session_id(value, max_bytes)?;
    Ok(Some(value.to_owned()))
}

fn parse_protocol_error(
    request_id: u64,
    session_id: Option<String>,
    value: &Value,
) -> Result<CdpProtocolError, CdpError> {
    let object = value.as_object().ok_or(CdpError::MalformedMessage)?;
    let code = object
        .get("code")
        .and_then(Value::as_i64)
        .ok_or(CdpError::MalformedMessage)?;
    let message = object
        .get("message")
        .and_then(Value::as_str)
        .filter(|message| !message.is_empty())
        .ok_or(CdpError::MalformedMessage)?;
    Ok(CdpProtocolError {
        request_id,
        session_id,
        code,
        message: message.to_owned(),
        data: object.get("data").cloned(),
    })
}

fn required_string<'a>(result: &'a Value, field: &'static str) -> Result<&'a str, CdpError> {
    result
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(CdpError::InvalidResponse(field))
}

fn required_bounded_response_string(
    result: &Value,
    field: &'static str,
    max_message_bytes: usize,
) -> Result<String, CdpError> {
    let value = required_string(result, field)?;
    if value.is_empty() || value.len() > max_message_bytes || value.chars().any(char::is_control) {
        return Err(CdpError::InvalidResponse(field));
    }
    Ok(value.to_owned())
}

fn required_dom_node_id(result: &Value) -> Result<u64, CdpError> {
    let root = result
        .get("root")
        .and_then(Value::as_object)
        .ok_or(CdpError::InvalidResponse("root"))?;
    let node_id = root
        .get("nodeId")
        .and_then(Value::as_u64)
        .filter(|node_id| *node_id > 0)
        .ok_or(CdpError::InvalidResponse("nodeId"))?;
    if node_id > i64::MAX as u64 {
        return Err(CdpError::InvalidResponse("nodeId"));
    }
    Ok(node_id)
}

fn checked_dom_node_id(node_id: u64) -> Result<i64, CdpError> {
    i64::try_from(node_id)
        .ok()
        .filter(|node_id| *node_id > 0)
        .ok_or(CdpError::InvalidInput)
}

fn required_bounded_document_string(
    result: &Value,
    field: &'static str,
    max_message_bytes: usize,
) -> Result<String, CdpError> {
    let value = required_string(result, field)?;
    validate_text(value, max_message_bytes).map_err(|_| CdpError::InvalidResponse(field))?;
    Ok(value.to_owned())
}

fn optional_response_string(
    result: &Value,
    field: &'static str,
) -> Result<Option<String>, CdpError> {
    match result.get(field) {
        None => Ok(None),
        Some(Value::String(value)) if !value.is_empty() => Ok(Some(value.clone())),
        Some(_) => Err(CdpError::InvalidResponse(field)),
    }
}

fn required_accessibility_identifier(
    object: &Map<String, Value>,
    field: &'static str,
    max_message_bytes: usize,
) -> Result<String, CdpError> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or(CdpError::InvalidResponse(field))?;
    bounded_accessibility_identifier(value, field, max_message_bytes)
}

fn optional_accessibility_identifier(
    object: &Map<String, Value>,
    field: &'static str,
    max_message_bytes: usize,
) -> Result<Option<String>, CdpError> {
    match object.get(field) {
        None => Ok(None),
        Some(Value::String(value)) => {
            bounded_accessibility_identifier(value, field, max_message_bytes).map(Some)
        }
        Some(_) => Err(CdpError::InvalidResponse(field)),
    }
}

fn bounded_accessibility_identifier(
    value: &str,
    field: &'static str,
    max_message_bytes: usize,
) -> Result<String, CdpError> {
    if value.is_empty() || value.len() > max_message_bytes || value.chars().any(char::is_control) {
        return Err(CdpError::InvalidResponse(field));
    }
    Ok(value.to_owned())
}

fn required_accessibility_value(
    object: &Map<String, Value>,
    name: &'static str,
    field: &'static str,
    max_message_bytes: usize,
) -> Result<CdpAccessibilityValue, CdpError> {
    let value = object.get(name).ok_or(CdpError::InvalidResponse(field))?;
    CdpAccessibilityValue::from_response(value, field, max_message_bytes)
}

fn optional_accessibility_value(
    object: &Map<String, Value>,
    name: &'static str,
    field: &'static str,
    max_message_bytes: usize,
) -> Result<Option<CdpAccessibilityValue>, CdpError> {
    match object.get(name) {
        None => Ok(None),
        Some(value) => {
            CdpAccessibilityValue::from_response(value, field, max_message_bytes).map(Some)
        }
    }
}

fn select_first_page_target(result: &Value) -> Result<String, CdpError> {
    let targets = result
        .get("targetInfos")
        .and_then(Value::as_array)
        .ok_or(CdpError::InvalidResponse("targetInfos"))?;
    let mut first_page = None;
    for target in targets {
        let object = target
            .as_object()
            .ok_or(CdpError::InvalidResponse("targetInfos"))?;
        if object.get("type").and_then(Value::as_str) != Some("page") {
            continue;
        }
        let target_id = object
            .get("targetId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(CdpError::InvalidResponse("targetId"))?;
        if object.get("url").and_then(Value::as_str) == Some("about:blank") {
            return Ok(target_id.to_owned());
        }
        if first_page.is_none() {
            first_page = Some(target_id.to_owned());
        }
    }
    first_page.ok_or(CdpError::InvalidResponse("page target"))
}

fn map_socket_error(error: TungsteniteError) -> CdpError {
    match error {
        TungsteniteError::ConnectionClosed | TungsteniteError::AlreadyClosed => CdpError::Closed,
        TungsteniteError::Capacity(_) => CdpError::MessageTooLarge,
        TungsteniteError::WriteBufferFull(_) => CdpError::Backpressure,
        other => CdpError::WebSocket(other.to_string()),
    }
}

fn error_is_terminal(error: &CdpError) -> bool {
    matches!(
        error,
        CdpError::Closed | CdpError::MessageTooLarge | CdpError::WebSocket(_)
    )
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::future::Future;
    use std::io;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use futures_util::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;
    use tokio_tungstenite::WebSocketStream;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::protocol::Message;

    use super::{
        CdpAccessibilityTree, CdpAccessibilityValue, CdpBrowserVersion, CdpError, CdpLimits,
        CdpSession, checked_dom_node_id, parse_protocol_error, required_bounded_document_string,
        required_dom_node_id, select_first_page_target, validate_endpoint, validate_method,
        validate_session_id, validate_text,
    };

    type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;
    type MockSocket = WebSocketStream<TcpStream>;

    async fn start_mock<F, Fut>(handler: F) -> TestResult<(String, JoinHandle<TestResult>)>
    where
        F: FnOnce(MockSocket) -> Fut + Send + 'static,
        Fut: Future<Output = TestResult> + Send + 'static,
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let task: JoinHandle<TestResult> = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let socket = accept_async(stream).await?;
            handler(socket).await
        });
        Ok((format!("ws://{address}/devtools/browser/mock"), task))
    }

    async fn receive_json(socket: &mut MockSocket) -> TestResult<Value> {
        loop {
            let message = socket
                .next()
                .await
                .ok_or_else(|| io::Error::other("mock client disconnected"))??;
            match message {
                Message::Text(text) => return Ok(serde_json::from_str(&text)?),
                Message::Ping(_) | Message::Pong(_) => socket.flush().await?,
                Message::Close(_) => return Err(io::Error::other("mock client closed").into()),
                Message::Binary(_) | Message::Frame(_) => {
                    return Err(io::Error::other("mock received unexpected frame").into());
                }
            }
        }
    }

    fn request_id(request: &Value) -> TestResult<u64> {
        request
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| io::Error::other("request omitted numeric id").into())
    }

    fn accessibility_tree_fixture() -> Value {
        json!({
            "nodes": [
                {
                    "nodeId": "root",
                    "role": { "type": "role", "value": "RootWebArea" },
                    "name": { "type": "computedString", "value": "TermGlide accessibility fixture" },
                    "childIds": ["button"]
                },
                {
                    "nodeId": "button",
                    "parentId": "root",
                    "role": { "type": "role", "value": "button" },
                    "name": { "type": "computedString", "value": "Continue" },
                    "properties": [
                        {
                            "name": "pressed",
                            "value": { "type": "boolean", "value": true }
                        },
                        {
                            "name": "disabled",
                            "value": { "type": "boolean", "value": false }
                        },
                        {
                            "name": "labelledby",
                            "value": {
                                "type": "nodeList",
                                "value": [{ "backendDOMNodeId": 9, "text": "Continue" }]
                            }
                        }
                    ]
                }
            ]
        })
    }

    #[tokio::test]
    async fn cdp_accessibility_tree_is_typed_and_ignores_complex_properties() -> TestResult {
        let (endpoint, server) = start_mock(|mut socket| async move {
            let enable = receive_json(&mut socket).await?;
            let enable_id = request_id(&enable)?;
            if enable.get("method").and_then(Value::as_str) != Some("Accessibility.enable")
                || enable.get("params") != Some(&json!({}))
            {
                return Err(io::Error::other("unexpected Accessibility.enable request").into());
            }
            socket
                .send(Message::Text(
                    json!({ "id": enable_id, "result": {} }).to_string().into(),
                ))
                .await?;

            let tree = receive_json(&mut socket).await?;
            let tree_id = request_id(&tree)?;
            if tree.get("method").and_then(Value::as_str) != Some("Accessibility.getFullAXTree")
                || tree.get("params") != Some(&json!({}))
            {
                return Err(
                    io::Error::other("unexpected Accessibility.getFullAXTree request").into(),
                );
            }
            socket
                .send(Message::Text(
                    json!({ "id": tree_id, "result": accessibility_tree_fixture() })
                        .to_string()
                        .into(),
                ))
                .await?;
            Ok(())
        })
        .await?;

        let mut session = CdpSession::connect(endpoint, CdpLimits::default()).await?;
        session.accessibility_enable().await?;
        let tree = session.accessibility_tree().await?;
        assert_eq!(tree.nodes.len(), 2);
        assert_eq!(tree.nodes[0].role, "RootWebArea");
        assert_eq!(
            tree.nodes[0].name.as_deref(),
            Some("TermGlide accessibility fixture")
        );
        assert_eq!(tree.nodes[1].role, "button");
        assert_eq!(tree.nodes[1].name.as_deref(), Some("Continue"));
        assert_eq!(
            tree.nodes[1].states,
            vec![
                super::CdpAccessibilityState {
                    name: "disabled".to_owned(),
                    value: CdpAccessibilityValue::Boolean(false),
                },
                super::CdpAccessibilityState {
                    name: "pressed".to_owned(),
                    value: CdpAccessibilityValue::Boolean(true),
                },
            ]
        );
        server.await??;
        Ok(())
    }

    #[test]
    fn cdp_accessibility_tree_rejects_quota_depth_and_malformed_responses() {
        let fixture = accessibility_tree_fixture();

        let node_limited = CdpLimits {
            max_accessibility_nodes: 1,
            ..CdpLimits::default()
        };
        assert!(matches!(
            CdpAccessibilityTree::from_response(&fixture, &node_limited),
            Err(CdpError::AccessibilityNodeLimit { limit: 1 })
        ));

        let depth_limited = CdpLimits {
            max_accessibility_depth: 1,
            ..CdpLimits::default()
        };
        assert!(matches!(
            CdpAccessibilityTree::from_response(&fixture, &depth_limited),
            Err(CdpError::AccessibilityDepthLimit { limit: 1 })
        ));

        let size_limited = CdpLimits {
            max_message_bytes: 16,
            ..CdpLimits::default()
        };
        let oversized_name = json!({
            "nodes": [{
                "nodeId": "root",
                "role": { "type": "role", "value": "RootWebArea" },
                "name": {
                    "type": "computedString",
                    "value": "this accessibility name exceeds the bound"
                }
            }]
        });
        assert!(matches!(
            CdpAccessibilityTree::from_response(&oversized_name, &size_limited),
            Err(CdpError::InvalidResponse("accessibility name"))
        ));

        let malformed = json!({
            "nodes": [{
                "nodeId": "root",
                "role": { "type": "role", "value": "RootWebArea" },
                "childIds": ["missing"]
            }]
        });
        assert!(matches!(
            CdpAccessibilityTree::from_response(&malformed, &CdpLimits::default()),
            Err(CdpError::InvalidResponse("accessibility childIds"))
        ));
    }

    #[tokio::test]
    async fn cdp_accessibility_helpers_share_timeout_and_explicit_close_semantics() -> TestResult {
        let (endpoint, timeout_server) = start_mock(|mut socket| async move {
            let request = receive_json(&mut socket).await?;
            if request.get("method").and_then(Value::as_str) != Some("Accessibility.enable") {
                return Err(io::Error::other("unexpected accessibility timeout request").into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .await?;
        let limits = CdpLimits {
            operation_timeout: Duration::from_millis(25),
            ..CdpLimits::default()
        };
        let mut timed_out = CdpSession::connect(endpoint, limits).await?;
        assert!(matches!(
            timed_out.accessibility_enable().await,
            Err(CdpError::Timeout)
        ));
        timeout_server.await??;

        let (endpoint, close_server) = start_mock(|mut socket| async move {
            let message = socket
                .next()
                .await
                .ok_or_else(|| io::Error::other("client disconnected before close"))??;
            if !matches!(message, Message::Close(_)) {
                return Err(io::Error::other("client did not send a close frame").into());
            }
            Ok(())
        })
        .await?;
        let mut closed = CdpSession::connect(endpoint, CdpLimits::default()).await?;
        closed.close().await?;
        assert!(matches!(
            closed.accessibility_tree().await,
            Err(CdpError::Closed)
        ));
        close_server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn cdp_correlates_normal_response_with_monotonic_id() -> TestResult {
        let (endpoint, server) = start_mock(|mut socket| async move {
            let request = receive_json(&mut socket).await?;
            if request.get("method").and_then(Value::as_str) != Some("Browser.getVersion") {
                return Err(io::Error::other("unexpected method").into());
            }
            let id = request_id(&request)?;
            socket
                .send(Message::Text(
                    json!({ "id": id, "result": { "product": "mock" } })
                        .to_string()
                        .into(),
                ))
                .await?;
            Ok(())
        })
        .await?;
        let mut session = CdpSession::connect(endpoint, CdpLimits::default()).await?;
        let reply = session.send("Browser.getVersion", Value::Null).await?;
        assert_eq!(reply.id, 1);
        assert_eq!(reply.result, json!({ "product": "mock" }));
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn cdp_browser_version_parses_full_bounded_response() -> TestResult {
        let (endpoint, server) = start_mock(|mut socket| async move {
            let request = receive_json(&mut socket).await?;
            if request.get("method").and_then(Value::as_str) != Some("Browser.getVersion")
                || request.get("params").is_some()
            {
                return Err(io::Error::other("unexpected browser version request").into());
            }
            let id = request_id(&request)?;
            socket
                .send(Message::Text(
                    json!({
                        "id": id,
                        "result": {
                            "protocolVersion": "1.3",
                            "product": "Chrome/123.0.0.0",
                            "revision": "deadbeef",
                            "userAgent": "TermGlide test agent",
                            "jsVersion": "12.3.4"
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await?;
            Ok(())
        })
        .await?;
        let mut session = CdpSession::connect(endpoint, CdpLimits::default()).await?;
        let version = session.browser_version().await?;
        assert_eq!(
            version,
            CdpBrowserVersion {
                protocol_version: "1.3".to_owned(),
                product: "Chrome/123.0.0.0".to_owned(),
                revision: "deadbeef".to_owned(),
                user_agent: "TermGlide test agent".to_owned(),
                js_version: "12.3.4".to_owned(),
            }
        );
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn cdp_browser_version_rejects_missing_or_wrong_typed_fields() -> TestResult {
        for (field, result) in [
            (
                "protocolVersion",
                json!({
                    "product": "Chrome/123",
                    "revision": "deadbeef",
                    "userAgent": "agent",
                    "jsVersion": "12.3"
                }),
            ),
            (
                "product",
                json!({
                    "protocolVersion": "1.3",
                    "product": 123,
                    "revision": "deadbeef",
                    "userAgent": "agent",
                    "jsVersion": "12.3"
                }),
            ),
            (
                "product",
                json!({
                    "protocolVersion": "1.3",
                    "product": "Chrome\u{001b}[31m",
                    "revision": "deadbeef",
                    "userAgent": "agent",
                    "jsVersion": "12.3"
                }),
            ),
            (
                "revision",
                json!({
                    "protocolVersion": "1.3",
                    "product": "Chrome/123",
                    "userAgent": "agent",
                    "jsVersion": "12.3"
                }),
            ),
            (
                "userAgent",
                json!({
                    "protocolVersion": "1.3",
                    "product": "Chrome/123",
                    "revision": "deadbeef",
                    "userAgent": false,
                    "jsVersion": "12.3"
                }),
            ),
            (
                "jsVersion",
                json!({
                    "protocolVersion": "1.3",
                    "product": "Chrome/123",
                    "revision": "deadbeef",
                    "userAgent": "agent"
                }),
            ),
        ] {
            let (endpoint, server) = start_mock(move |mut socket| async move {
                let request = receive_json(&mut socket).await?;
                let id = request_id(&request)?;
                socket
                    .send(Message::Text(
                        json!({ "id": id, "result": result }).to_string().into(),
                    ))
                    .await?;
                Ok(())
            })
            .await?;
            let mut session = CdpSession::connect(endpoint, CdpLimits::default()).await?;
            assert!(matches!(
                session.browser_version().await,
                Err(CdpError::InvalidResponse(actual)) if actual == field
            ));
            server.await??;
        }
        Ok(())
    }

    #[test]
    fn dom_response_helpers_require_positive_bounded_values() -> TestResult {
        assert_eq!(
            required_dom_node_id(&json!({ "root": { "nodeId": 7 } }))?,
            7
        );
        for result in [
            json!({}),
            json!({ "root": [] }),
            json!({ "root": { "nodeId": 0 } }),
            json!({ "root": { "nodeId": -1 } }),
            json!({ "root": { "nodeId": u64::MAX } }),
        ] {
            assert!(matches!(
                required_dom_node_id(&result),
                Err(CdpError::InvalidResponse("root" | "nodeId"))
            ));
        }
        assert!(matches!(
            checked_dom_node_id(0),
            Err(CdpError::InvalidInput)
        ));
        assert!(matches!(
            checked_dom_node_id(u64::MAX),
            Err(CdpError::InvalidInput)
        ));

        let outer_html = "<html>\n<body>fixture</body>\n</html>";
        assert_eq!(
            required_bounded_document_string(
                &json!({ "outerHTML": outer_html }),
                "outerHTML",
                outer_html.len(),
            )?,
            outer_html
        );
        for result in [
            json!({}),
            json!({ "outerHTML": "" }),
            json!({ "outerHTML": false }),
            json!({ "outerHTML": "\u{0}" }),
        ] {
            assert!(matches!(
                required_bounded_document_string(&result, "outerHTML", 128),
                Err(CdpError::InvalidResponse("outerHTML"))
            ));
        }
        assert!(matches!(
            required_bounded_document_string(&json!({ "outerHTML": "too long" }), "outerHTML", 1,),
            Err(CdpError::InvalidResponse("outerHTML"))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn cdp_browser_version_keeps_endpoints_loopback_and_credential_free() -> TestResult {
        for endpoint in [
            "ws://192.0.2.1/devtools/browser/version",
            "ws://10.0.0.1/devtools/browser/version",
            "ws://user:secret@127.0.0.1/devtools/browser/version",
        ] {
            assert!(matches!(
                CdpSession::connect(endpoint, CdpLimits::default()).await,
                Err(CdpError::InvalidEndpoint)
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn cdp_queues_interleaved_events_before_the_matching_response() -> TestResult {
        let (endpoint, server) = start_mock(|mut socket| async move {
            let request = receive_json(&mut socket).await?;
            let id = request_id(&request)?;
            socket
                .send(Message::Text(
                    json!({
                        "method": "Page.loadEventFired",
                        "params": { "timestamp": 1.25 }
                    })
                    .to_string()
                    .into(),
                ))
                .await?;
            socket
                .send(Message::Text(
                    json!({ "id": id, "result": {} }).to_string().into(),
                ))
                .await?;
            Ok(())
        })
        .await?;
        let mut session = CdpSession::connect(endpoint, CdpLimits::default()).await?;
        session.page_enable().await?;
        let event = session
            .next_event()
            .ok_or_else(|| io::Error::other("interleaved event was not retained"))?;
        assert_eq!(event.method, "Page.loadEventFired");
        assert_eq!(event.params, json!({ "timestamp": 1.25 }));
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn cdp_surfaces_browser_protocol_errors() -> TestResult {
        let (endpoint, server) = start_mock(|mut socket| async move {
            let request = receive_json(&mut socket).await?;
            let id = request_id(&request)?;
            socket
                .send(Message::Text(json!({
                    "id": id,
                    "error": { "code": -32000, "message": "denied", "data": { "source": "mock" } }
                }).to_string().into()))
                .await?;
            Ok(())
        })
        .await?;
        let mut session = CdpSession::connect(endpoint, CdpLimits::default()).await?;
        let error = session
            .send("Browser.getVersion", Value::Null)
            .await
            .err()
            .ok_or_else(|| io::Error::other("protocol error unexpectedly succeeded"))?;
        assert!(matches!(
            error,
            CdpError::Protocol(ref error)
                if error.request_id == 1 && error.code == -32000 && error.message == "denied"
        ));
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn cdp_times_out_and_reports_peer_close() -> TestResult {
        let (endpoint, timeout_server) = start_mock(|mut socket| async move {
            let _request = receive_json(&mut socket).await?;
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .await?;
        let limits = CdpLimits {
            operation_timeout: Duration::from_millis(25),
            ..CdpLimits::default()
        };
        let mut session = CdpSession::connect(endpoint, limits).await?;
        assert!(matches!(
            session.send("Browser.getVersion", Value::Null).await,
            Err(CdpError::Timeout)
        ));
        timeout_server.await??;

        let (endpoint, close_server) = start_mock(|mut socket| async move {
            let _request = receive_json(&mut socket).await?;
            socket.send(Message::Close(None)).await?;
            Ok(())
        })
        .await?;
        let mut session = CdpSession::connect(endpoint, CdpLimits::default()).await?;
        assert!(matches!(
            session.send("Browser.getVersion", Value::Null).await,
            Err(CdpError::Closed)
        ));
        assert!(!session.is_connected());
        close_server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn cdp_reconnects_after_a_closed_transport_without_reusing_ids() -> TestResult {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let endpoint = format!("ws://{address}/devtools/browser/recovery");
        let server: JoinHandle<TestResult> = tokio::spawn(async move {
            let (first_stream, _) = listener.accept().await?;
            let mut first = accept_async(first_stream).await?;
            let _first_request = receive_json(&mut first).await?;
            first.send(Message::Close(None)).await?;
            drop(first);

            let (second_stream, _) = listener.accept().await?;
            let mut second = accept_async(second_stream).await?;
            let second_request = receive_json(&mut second).await?;
            let second_id = request_id(&second_request)?;
            second
                .send(Message::Text(
                    json!({ "id": second_id, "result": { "recovered": true } })
                        .to_string()
                        .into(),
                ))
                .await?;
            Ok(())
        });
        let mut session = CdpSession::connect(endpoint, CdpLimits::default()).await?;
        assert!(matches!(
            session.send("Browser.getVersion", Value::Null).await,
            Err(CdpError::Closed)
        ));
        session.reconnect().await?;
        let recovered = session.send("Browser.getVersion", Value::Null).await?;
        assert_eq!(recovered.id, 2);
        assert_eq!(recovered.result, json!({ "recovered": true }));
        server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn cdp_rejects_binary_oversized_and_malformed_websocket_messages() -> TestResult {
        let (endpoint, oversized_server) = start_mock(|mut socket| async move {
            let _request = receive_json(&mut socket).await?;
            socket.send(Message::Text("x".repeat(256).into())).await?;
            Ok(())
        })
        .await?;
        let limits = CdpLimits {
            max_message_bytes: 64,
            ..CdpLimits::default()
        };
        let mut session = CdpSession::connect(endpoint, limits).await?;
        assert!(matches!(
            session.send("Browser.getVersion", Value::Null).await,
            Err(CdpError::MessageTooLarge)
        ));
        oversized_server.await??;

        let (endpoint, malformed_server) = start_mock(|mut socket| async move {
            let _request = receive_json(&mut socket).await?;
            socket.send(Message::Text("{not-json".into())).await?;
            Ok(())
        })
        .await?;
        let mut session = CdpSession::connect(endpoint, CdpLimits::default()).await?;
        assert!(matches!(
            session.send("Browser.getVersion", Value::Null).await,
            Err(CdpError::MalformedMessage)
        ));
        malformed_server.await??;

        let (endpoint, binary_server) = start_mock(|mut socket| async move {
            let _request = receive_json(&mut socket).await?;
            socket.send(Message::Binary(vec![1, 2, 3].into())).await?;
            Ok(())
        })
        .await?;
        let mut session = CdpSession::connect(endpoint, CdpLimits::default()).await?;
        assert!(matches!(
            session.send("Browser.getVersion", Value::Null).await,
            Err(CdpError::BinaryMessage)
        ));
        binary_server.await??;
        Ok(())
    }

    #[tokio::test]
    async fn cdp_rejects_remote_and_credentialed_endpoints_before_connecting() -> TestResult {
        for endpoint in [
            "ws://192.0.2.1/devtools/browser/mock",
            "ws://10.0.0.1/devtools/browser/mock",
            "ws://user:secret@127.0.0.1/devtools/browser/mock",
        ] {
            assert!(matches!(
                CdpSession::connect(endpoint, CdpLimits::default()).await,
                Err(CdpError::InvalidEndpoint)
            ));
        }
        Ok(())
    }

    #[tokio::test]
    async fn cdp_attach_first_page_scopes_helpers_with_the_flattened_session_id() -> TestResult {
        let (endpoint, server) = start_mock(|mut socket| async move {
            let targets = receive_json(&mut socket).await?;
            let targets_id = request_id(&targets)?;
            if targets.get("method").and_then(Value::as_str) != Some("Target.getTargets") {
                return Err(io::Error::other("first command was not Target.getTargets").into());
            }
            socket
                .send(Message::Text(
                    json!({
                        "id": targets_id,
                        "result": {
                            "targetInfos": [
                                { "targetId": "other", "type": "page", "url": "mock-page" },
                                { "targetId": "blank", "type": "page", "url": "about:blank" }
                            ]
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await?;

            let attach = receive_json(&mut socket).await?;
            let attach_id = request_id(&attach)?;
            if attach.get("method").and_then(Value::as_str) != Some("Target.attachToTarget")
                || attach.pointer("/params/targetId").and_then(Value::as_str) != Some("blank")
                || attach.pointer("/params/flatten").and_then(Value::as_bool) != Some(true)
            {
                return Err(
                    io::Error::other("attach request did not select the blank page").into(),
                );
            }
            socket
                .send(Message::Text(
                    json!({
                        "id": attach_id,
                        "result": { "sessionId": "target-session" }
                    })
                    .to_string()
                    .into(),
                ))
                .await?;

            let page_enable = receive_json(&mut socket).await?;
            let page_enable_id = request_id(&page_enable)?;
            if page_enable.get("method").and_then(Value::as_str) != Some("Page.enable")
                || page_enable.get("sessionId").and_then(Value::as_str) != Some("target-session")
            {
                return Err(io::Error::other("Page.enable omitted its target session ID").into());
            }
            socket
                .send(Message::Text(
                    json!({
                        "id": page_enable_id,
                        "sessionId": "target-session",
                        "result": {}
                    })
                    .to_string()
                    .into(),
                ))
                .await?;

            let accessibility_enable = receive_json(&mut socket).await?;
            let accessibility_enable_id = request_id(&accessibility_enable)?;
            if accessibility_enable.get("method").and_then(Value::as_str)
                != Some("Accessibility.enable")
                || accessibility_enable
                    .get("sessionId")
                    .and_then(Value::as_str)
                    != Some("target-session")
            {
                return Err(
                    io::Error::other("Accessibility.enable omitted its target session ID").into(),
                );
            }
            socket
                .send(Message::Text(
                    json!({
                        "id": accessibility_enable_id,
                        "sessionId": "target-session",
                        "result": {}
                    })
                    .to_string()
                    .into(),
                ))
                .await?;

            let accessibility_tree = receive_json(&mut socket).await?;
            let accessibility_tree_id = request_id(&accessibility_tree)?;
            if accessibility_tree.get("method").and_then(Value::as_str)
                != Some("Accessibility.getFullAXTree")
                || accessibility_tree.get("sessionId").and_then(Value::as_str)
                    != Some("target-session")
            {
                return Err(io::Error::other(
                    "Accessibility.getFullAXTree omitted its target session ID",
                )
                .into());
            }
            socket
                .send(Message::Text(
                    json!({
                        "id": accessibility_tree_id,
                        "sessionId": "target-session",
                        "result": accessibility_tree_fixture()
                    })
                    .to_string()
                    .into(),
                ))
                .await?;
            Ok(())
        })
        .await?;
        let mut session = CdpSession::connect(endpoint, CdpLimits::default()).await?;
        let mut target = session.attach_first_page().await?;
        assert_eq!(target.session_id(), "target-session");
        target.page_enable().await?;
        target.accessibility_enable().await?;
        assert_eq!(target.accessibility_tree().await?.nodes[1].role, "button");
        server.await??;
        Ok(())
    }

    #[test]
    fn validate_endpoint_accepts_loopback_only() -> TestResult {
        let limits = CdpLimits::default();
        assert!(validate_endpoint("ws://127.0.0.1:49213/devtools/browser/x", &limits).is_ok());
        assert!(validate_endpoint("ws://localhost/devtools/browser/x", &limits).is_ok());
        assert!(validate_endpoint("wss://[::1]/devtools/browser/x", &limits).is_ok());
        assert!(validate_endpoint("ws://example.com/devtools", &limits).is_err());
        assert!(validate_endpoint("ws://10.0.0.5:9222/devtools", &limits).is_err());
        assert!(validate_endpoint("ws://user:pass@127.0.0.1/devtools", &limits).is_err());
        assert!(validate_endpoint("http://127.0.0.1:9222/devtools", &limits).is_err());
        assert!(validate_endpoint("", &limits).is_err());
        assert!(validate_endpoint("ws://127.0.0.1/\0", &limits).is_err());
        Ok(())
    }

    #[test]
    fn validate_method_rejects_malformed_input() -> TestResult {
        assert!(validate_method("Page.navigate", 1024).is_ok());
        assert!(validate_method("", 1024).is_err());
        assert!(validate_method("Page.navigate", 5).is_err());
        assert!(validate_method("Page\nnavigate", 1024).is_err());
        Ok(())
    }

    #[test]
    fn validate_session_id_rejects_control_chars() -> TestResult {
        assert!(validate_session_id("abc-123", 1024).is_ok());
        assert!(validate_session_id("", 1024).is_err());
        assert!(validate_session_id("abc\n", 1024).is_err());
        Ok(())
    }

    #[test]
    fn validate_text_enforces_bounds() -> TestResult {
        assert!(validate_text("hello", 1024).is_ok());
        assert!(validate_text("hello", 3).is_err());
        assert!(validate_text("a\0b", 1024).is_err());
        Ok(())
    }

    #[test]
    fn select_first_page_target_prefers_blank_page() -> TestResult {
        let result = json!({
            "targetInfos": [
                { "type": "page", "targetId": "p1", "url": "https://example.com" },
                { "type": "page", "targetId": "p2", "url": "about:blank" },
                { "type": "background_page", "targetId": "b1", "url": "about:blank" }
            ]
        });
        let target_id = select_first_page_target(&result)?;
        assert_eq!(target_id, "p2");
        Ok(())
    }

    #[test]
    fn select_first_page_target_falls_back_to_first_page() -> TestResult {
        let result = json!({
            "targetInfos": [
                { "type": "service_worker", "targetId": "sw1", "url": "https://example.com" },
                { "type": "page", "targetId": "p1", "url": "https://example.com" }
            ]
        });
        let target_id = select_first_page_target(&result)?;
        assert_eq!(target_id, "p1");
        Ok(())
    }

    #[test]
    fn select_first_page_target_rejects_no_page() -> TestResult {
        let result = json!({ "targetInfos": [ { "type": "other", "targetId": "x", "url": "about:blank" } ] });
        assert!(select_first_page_target(&result).is_err());
        Ok(())
    }

    #[test]
    fn checked_dom_node_id_rejects_nonpositive_and_overflow() -> TestResult {
        let node_id = checked_dom_node_id(42)?;
        assert_eq!(node_id, 42);
        assert!(checked_dom_node_id(0).is_err());
        assert!(checked_dom_node_id(u64::MAX).is_err());
        Ok(())
    }

    #[test]
    fn required_dom_node_id_requires_positive_root() -> TestResult {
        let valid = json!({ "root": { "nodeId": 7 } });
        let node_id = required_dom_node_id(&valid)?;
        assert_eq!(node_id, 7);
        let zero = json!({ "root": { "nodeId": 0 } });
        assert!(required_dom_node_id(&zero).is_err());
        let missing = json!({ "other": {} });
        assert!(required_dom_node_id(&missing).is_err());
        Ok(())
    }

    #[test]
    fn parse_protocol_error_requires_code_and_message() -> TestResult {
        let value = json!({ "code": -32000, "message": "boom", "data": { "k": 1 } });
        let error = parse_protocol_error(9, Some("s".to_owned()), &value)?;
        assert_eq!(error.code, -32000);
        assert_eq!(error.message, "boom");
        assert_eq!(error.request_id, 9);
        let malformed = json!({ "code": -32000 });
        assert!(parse_protocol_error(9, None, &malformed).is_err());
        Ok(())
    }
}
