//! Typed terminal-input translation for an attached external CDP page target.
//!
//! The adapter stores geometry and limits only. It receives already-decoded terminal events and
//! never accepts or retains raw terminal byte sequences.

use serde_json::{Map, Value, json};
use tg_core::Cancellation;
use tg_network::{CdpError, CdpTargetSession};
use tg_terminal::{InputEvent, KeyCode, KeyEventKind, Modifiers, MouseButton};
use thiserror::Error;

const MAX_CSS_COORDINATE: f64 = 10_000_000.0;
const MAX_WHEEL_DELTA: f64 = 1_000_000.0;
const MAX_TEXT_BYTES: usize = 1024 * 1024;
const MAX_ACTIONS: usize = 32;

/// Validated terminal-to-CSS geometry for one attached external page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExternalInputGeometry {
    terminal_columns: u16,
    terminal_rows: u16,
    css_width: f64,
    css_height: f64,
}

impl ExternalInputGeometry {
    /// Validates the terminal grid and CSS viewport used for pointer translation.
    pub fn new(
        terminal_columns: u16,
        terminal_rows: u16,
        css_width: f64,
        css_height: f64,
    ) -> Result<Self, ExternalInputError> {
        if terminal_columns == 0
            || terminal_rows == 0
            || !css_width.is_finite()
            || !css_height.is_finite()
            || css_width <= 0.0
            || css_height <= 0.0
            || css_width > MAX_CSS_COORDINATE
            || css_height > MAX_CSS_COORDINATE
        {
            return Err(ExternalInputError::InvalidGeometry);
        }
        Ok(Self {
            terminal_columns,
            terminal_rows,
            css_width,
            css_height,
        })
    }

    /// Returns the terminal column count.
    pub const fn terminal_columns(self) -> u16 {
        self.terminal_columns
    }

    /// Returns the terminal row count.
    pub const fn terminal_rows(self) -> u16 {
        self.terminal_rows
    }

    /// Returns the CSS viewport width.
    pub const fn css_width(self) -> f64 {
        self.css_width
    }

    /// Returns the CSS viewport height.
    pub const fn css_height(self) -> f64 {
        self.css_height
    }

    /// Maps a terminal cell to the center of its corresponding CSS pixel region.
    pub fn cell_center(
        self,
        column: u16,
        row: u16,
    ) -> Result<ExternalInputPoint, ExternalInputError> {
        if column >= self.terminal_columns || row >= self.terminal_rows {
            return Err(ExternalInputError::TerminalCoordinate {
                column,
                row,
                columns: self.terminal_columns,
                rows: self.terminal_rows,
            });
        }
        let x = (f64::from(column) + 0.5) * self.css_width / f64::from(self.terminal_columns);
        let y = (f64::from(row) + 0.5) * self.css_height / f64::from(self.terminal_rows);
        if !x.is_finite()
            || !y.is_finite()
            || x.abs() > MAX_CSS_COORDINATE
            || y.abs() > MAX_CSS_COORDINATE
        {
            return Err(ExternalInputError::CssCoordinate);
        }
        Ok(ExternalInputPoint { x, y })
    }
}

/// Bounded configuration for one external input adapter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExternalInputLimits {
    pub max_text_bytes: usize,
    pub max_actions: usize,
    pub max_css_coordinate: f64,
    pub wheel_delta: f64,
}

impl ExternalInputLimits {
    /// Default bounds for one browser page target.
    pub const BROWSER_DEFAULT: Self = Self {
        max_text_bytes: 64 * 1024,
        max_actions: 4,
        max_css_coordinate: MAX_CSS_COORDINATE,
        wheel_delta: 120.0,
    };

    fn validate(self) -> Result<(), ExternalInputError> {
        if self.max_text_bytes == 0
            || self.max_text_bytes > MAX_TEXT_BYTES
            || self.max_actions == 0
            || self.max_actions > MAX_ACTIONS
            || !self.max_css_coordinate.is_finite()
            || self.max_css_coordinate <= 0.0
            || self.max_css_coordinate > MAX_CSS_COORDINATE
            || !self.wheel_delta.is_finite()
            || self.wheel_delta <= 0.0
            || self.wheel_delta > MAX_WHEEL_DELTA
        {
            return Err(ExternalInputError::InvalidLimits);
        }
        Ok(())
    }
}

impl Default for ExternalInputLimits {
    fn default() -> Self {
        Self::BROWSER_DEFAULT
    }
}

/// CSS coordinates derived from a valid terminal cell.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExternalInputPoint {
    pub x: f64,
    pub y: f64,
}

/// A key event type understood by the CDP Input domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalKeyEventKind {
    Down,
    Repeat,
    Up,
}

impl ExternalKeyEventKind {
    const fn cdp_type(self) -> &'static str {
        match self {
            Self::Down | Self::Repeat => "keyDown",
            Self::Up => "keyUp",
        }
    }

    const fn auto_repeat(self) -> bool {
        matches!(self, Self::Repeat)
    }
}

/// A pointer event type understood by the CDP Input domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalPointerKind {
    Move,
    Press,
    Release,
    Wheel,
}

impl ExternalPointerKind {
    const fn cdp_type(self) -> &'static str {
        match self {
            Self::Move => "mouseMoved",
            Self::Press => "mousePressed",
            Self::Release => "mouseReleased",
            Self::Wheel => "mouseWheel",
        }
    }
}

/// A mouse button accepted by CDP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalPointerButton {
    None,
    Left,
    Middle,
    Right,
}

impl ExternalPointerButton {
    const fn cdp_name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Left => "left",
            Self::Middle => "middle",
            Self::Right => "right",
        }
    }
}

/// One bounded, typed action to send to the attached CDP target.
#[derive(Debug, Clone, PartialEq)]
pub enum ExternalInputAction {
    InsertText {
        text: String,
    },
    Key {
        kind: ExternalKeyEventKind,
        key: String,
        modifiers: u8,
    },
    Pointer {
        kind: ExternalPointerKind,
        button: ExternalPointerButton,
        point: ExternalInputPoint,
        modifiers: u8,
        delta_x: f64,
        delta_y: f64,
    },
    Focus {
        focused: bool,
    },
}

/// Count of actions accepted by the CDP target for one decoded terminal event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalInputDispatch {
    sent_actions: usize,
}

impl ExternalInputDispatch {
    /// Returns the number of CDP actions sent after all cancellation checks.
    pub const fn sent_actions(self) -> usize {
        self.sent_actions
    }
}

/// Errors confined to the external page-input boundary.
#[derive(Debug, Error)]
pub enum ExternalInputError {
    #[error(
        "external input geometry must use nonzero terminal dimensions and finite positive CSS dimensions"
    )]
    InvalidGeometry,
    #[error("external input limits are invalid")]
    InvalidLimits,
    #[error("terminal cell {column},{row} is outside the {columns}x{rows} input grid")]
    TerminalCoordinate {
        column: u16,
        row: u16,
        columns: u16,
        rows: u16,
    },
    #[error("mapped CSS coordinate is invalid or exceeds the hard bound")]
    CssCoordinate,
    #[error("external input text uses {actual} bytes, exceeding its limit of {maximum}")]
    TextLimit { actual: usize, maximum: usize },
    #[error("external input text contains a NUL character")]
    TextContainsNul,
    #[error("terminal key cannot be represented by CDP")]
    UnsupportedKey,
    #[error("terminal modifier cannot be represented by CDP")]
    UnsupportedModifier,
    #[error("external input translation exceeded its action bound")]
    ActionLimit,
    #[error("external input dispatch was cancelled before sending CDP")]
    Cancelled,
    #[error(transparent)]
    Cdp(#[from] CdpError),
}

/// Stateless translation and dispatch boundary for a single external page target.
#[derive(Debug, Clone, Copy)]
pub struct ExternalInputAdapter {
    geometry: ExternalInputGeometry,
    limits: ExternalInputLimits,
}

impl ExternalInputAdapter {
    /// Creates an adapter with browser-default input bounds.
    pub fn new(geometry: ExternalInputGeometry) -> Result<Self, ExternalInputError> {
        Self::with_limits(geometry, ExternalInputLimits::default())
    }

    /// Creates an adapter with caller-selected, validated bounds.
    pub fn with_limits(
        geometry: ExternalInputGeometry,
        limits: ExternalInputLimits,
    ) -> Result<Self, ExternalInputError> {
        limits.validate()?;
        if geometry.css_width() > limits.max_css_coordinate
            || geometry.css_height() > limits.max_css_coordinate
        {
            return Err(ExternalInputError::CssCoordinate);
        }
        Ok(Self { geometry, limits })
    }

    /// Returns the geometry used to derive CDP pointer coordinates.
    pub const fn geometry(self) -> ExternalInputGeometry {
        self.geometry
    }

    /// Returns the input limits applied before CDP dispatch.
    pub const fn limits(self) -> ExternalInputLimits {
        self.limits
    }

    /// Translates a decoded terminal event into bounded CDP target actions.
    pub fn translate(
        &self,
        event: &InputEvent,
    ) -> Result<Vec<ExternalInputAction>, ExternalInputError> {
        let actions = match event {
            InputEvent::Paste(text) => self.text_actions(text)?,
            InputEvent::Focus(focused) => vec![ExternalInputAction::Focus { focused: *focused }],
            InputEvent::Key {
                code,
                modifiers,
                kind,
                shifted_key,
                text,
                ..
            } => self.key_actions(code, *modifiers, *kind, *shifted_key, text.as_deref())?,
            InputEvent::Mouse {
                button,
                column,
                row,
                pressed,
                modifiers,
            } => self.pointer_actions(*button, *column, *row, *pressed, *modifiers)?,
        };
        if actions.len() > self.limits.max_actions {
            return Err(ExternalInputError::ActionLimit);
        }
        Ok(actions)
    }

    /// Sends the translated action sequence to the already-attached CDP target session.
    pub async fn dispatch(
        &self,
        target: &mut CdpTargetSession<'_>,
        event: &InputEvent,
        cancellation: &Cancellation,
    ) -> Result<ExternalInputDispatch, ExternalInputError> {
        ensure_active(cancellation)?;
        let actions = self.translate(event)?;
        let mut sent_actions = 0usize;
        for action in actions {
            ensure_active(cancellation)?;
            send_action(target, action).await?;
            sent_actions = sent_actions
                .checked_add(1)
                .ok_or(ExternalInputError::ActionLimit)?;
        }
        Ok(ExternalInputDispatch { sent_actions })
    }

    fn text_actions(&self, text: &str) -> Result<Vec<ExternalInputAction>, ExternalInputError> {
        validate_text(text, self.limits)?;
        if text.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![ExternalInputAction::InsertText {
            text: text.to_owned(),
        }])
    }

    fn key_actions(
        &self,
        code: &KeyCode,
        modifiers: Modifiers,
        kind: KeyEventKind,
        shifted_key: Option<char>,
        text: Option<&str>,
    ) -> Result<Vec<ExternalInputAction>, ExternalInputError> {
        let key = cdp_key_name(code, shifted_key, text)?;
        let modifiers = cdp_modifier_mask(modifiers)?;
        let kind = match kind {
            KeyEventKind::Press => ExternalKeyEventKind::Down,
            KeyEventKind::Repeat => ExternalKeyEventKind::Repeat,
            KeyEventKind::Release => ExternalKeyEventKind::Up,
        };
        let mut actions = vec![ExternalInputAction::Key {
            kind,
            key,
            modifiers,
        }];
        if !matches!(kind, ExternalKeyEventKind::Up)
            && let Some(text) = text
        {
            actions.extend(self.text_actions(text)?);
        }
        Ok(actions)
    }

    fn pointer_actions(
        &self,
        button: MouseButton,
        column: u16,
        row: u16,
        pressed: bool,
        modifiers: Modifiers,
    ) -> Result<Vec<ExternalInputAction>, ExternalInputError> {
        let point = self.geometry.cell_center(column, row)?;
        if point.x.abs() > self.limits.max_css_coordinate
            || point.y.abs() > self.limits.max_css_coordinate
        {
            return Err(ExternalInputError::CssCoordinate);
        }
        let shifted_vertical_wheel = modifiers.contains(Modifiers::SHIFT)
            && matches!(button, MouseButton::WheelUp | MouseButton::WheelDown);
        let horizontal_wheel_delta = match button {
            MouseButton::WheelLeft => Some(-self.limits.wheel_delta),
            MouseButton::WheelRight => Some(self.limits.wheel_delta),
            MouseButton::WheelUp if shifted_vertical_wheel => Some(-self.limits.wheel_delta),
            MouseButton::WheelDown if shifted_vertical_wheel => Some(self.limits.wheel_delta),
            _ => None,
        };
        let modifiers = cdp_modifier_mask(modifiers)?;
        let action = match button {
            MouseButton::None => ExternalInputAction::Pointer {
                kind: ExternalPointerKind::Move,
                button: ExternalPointerButton::None,
                point,
                modifiers,
                delta_x: 0.0,
                delta_y: 0.0,
            },
            MouseButton::Left => {
                pointer_button_action(ExternalPointerButton::Left, point, modifiers, pressed)
            }
            MouseButton::Middle => {
                pointer_button_action(ExternalPointerButton::Middle, point, modifiers, pressed)
            }
            MouseButton::Right => {
                pointer_button_action(ExternalPointerButton::Right, point, modifiers, pressed)
            }
            MouseButton::WheelUp
            | MouseButton::WheelDown
            | MouseButton::WheelLeft
            | MouseButton::WheelRight
                if !pressed =>
            {
                return Ok(Vec::new());
            }
            MouseButton::WheelUp
            | MouseButton::WheelDown
            | MouseButton::WheelLeft
            | MouseButton::WheelRight => ExternalInputAction::Pointer {
                kind: ExternalPointerKind::Wheel,
                button: ExternalPointerButton::None,
                point,
                modifiers,
                delta_x: horizontal_wheel_delta.unwrap_or(0.0),
                delta_y: if horizontal_wheel_delta.is_some() {
                    0.0
                } else if matches!(button, MouseButton::WheelUp) {
                    -self.limits.wheel_delta
                } else {
                    self.limits.wheel_delta
                },
            },
        };
        Ok(vec![action])
    }
}

fn ensure_active(cancellation: &Cancellation) -> Result<(), ExternalInputError> {
    if cancellation.is_cancelled() {
        Err(ExternalInputError::Cancelled)
    } else {
        Ok(())
    }
}

fn validate_text(text: &str, limits: ExternalInputLimits) -> Result<(), ExternalInputError> {
    if text.len() > limits.max_text_bytes {
        return Err(ExternalInputError::TextLimit {
            actual: text.len(),
            maximum: limits.max_text_bytes,
        });
    }
    if text.contains('\0') {
        return Err(ExternalInputError::TextContainsNul);
    }
    Ok(())
}

fn cdp_key_name(
    code: &KeyCode,
    shifted_key: Option<char>,
    text: Option<&str>,
) -> Result<String, ExternalInputError> {
    match code {
        KeyCode::Character(character) if !character.is_control() => {
            Ok(printable_key(text, shifted_key, *character).to_string())
        }
        KeyCode::Enter => Ok("Enter".to_owned()),
        KeyCode::Escape => Ok("Escape".to_owned()),
        KeyCode::Backspace => Ok("Backspace".to_owned()),
        KeyCode::Tab => Ok("Tab".to_owned()),
        KeyCode::Up => Ok("ArrowUp".to_owned()),
        KeyCode::Down => Ok("ArrowDown".to_owned()),
        KeyCode::Left => Ok("ArrowLeft".to_owned()),
        KeyCode::Right => Ok("ArrowRight".to_owned()),
        KeyCode::Home => Ok("Home".to_owned()),
        KeyCode::End => Ok("End".to_owned()),
        KeyCode::PageUp => Ok("PageUp".to_owned()),
        KeyCode::PageDown => Ok("PageDown".to_owned()),
        KeyCode::Insert => Ok("Insert".to_owned()),
        KeyCode::Delete => Ok("Delete".to_owned()),
        KeyCode::Function(number) if (1..=24).contains(number) => Ok(format!("F{number}")),
        KeyCode::Character(_)
        | KeyCode::Function(_)
        | KeyCode::Functional(_)
        | KeyCode::Unidentified => Err(ExternalInputError::UnsupportedKey),
    }
}

fn printable_key(text: Option<&str>, shifted_key: Option<char>, fallback: char) -> char {
    if let Some(character) = text.and_then(single_printable_character) {
        character
    } else if let Some(character) = shifted_key.filter(|character| !character.is_control()) {
        character
    } else {
        fallback
    }
}

fn single_printable_character(value: &str) -> Option<char> {
    let mut characters = value.chars();
    let character = characters.next()?;
    (characters.next().is_none() && !character.is_control()).then_some(character)
}

fn cdp_modifier_mask(modifiers: Modifiers) -> Result<u8, ExternalInputError> {
    if modifiers.intersects(Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK) {
        return Err(ExternalInputError::UnsupportedModifier);
    }
    let mut mask = 0u8;
    if modifiers.contains(Modifiers::ALT) {
        mask |= 1;
    }
    if modifiers.contains(Modifiers::CONTROL) {
        mask |= 2;
    }
    if modifiers.intersects(Modifiers::SUPER | Modifiers::HYPER | Modifiers::META) {
        mask |= 4;
    }
    if modifiers.contains(Modifiers::SHIFT) {
        mask |= 8;
    }
    Ok(mask)
}

async fn send_action(
    target: &mut CdpTargetSession<'_>,
    action: ExternalInputAction,
) -> Result<(), ExternalInputError> {
    match action {
        ExternalInputAction::InsertText { text } => target.insert_text(&text).await?,
        ExternalInputAction::Key {
            kind,
            key,
            modifiers,
        } => {
            let mut parameters = Map::new();
            parameters.insert("type".to_owned(), Value::String(kind.cdp_type().to_owned()));
            parameters.insert("key".to_owned(), Value::String(key.clone()));
            parameters.insert("modifiers".to_owned(), Value::from(modifiers));
            parameters.insert("autoRepeat".to_owned(), Value::from(kind.auto_repeat()));
            if let Some((code, virtual_key)) = cdp_key_metadata(&key) {
                parameters.insert("code".to_owned(), Value::String(code));
                parameters.insert("windowsVirtualKeyCode".to_owned(), Value::from(virtual_key));
            }
            if !matches!(kind, ExternalKeyEventKind::Up)
                && let Some(text) = cdp_activation_text(&key)
            {
                parameters.insert("text".to_owned(), Value::String(text.to_owned()));
                parameters.insert("unmodifiedText".to_owned(), Value::String(text.to_owned()));
            }
            target
                .send("Input.dispatchKeyEvent", Value::Object(parameters))
                .await?;
        }
        ExternalInputAction::Pointer {
            kind,
            button,
            point,
            modifiers,
            delta_x,
            delta_y,
        } => {
            target
                .send(
                    "Input.dispatchMouseEvent",
                    json!({
                        "type": kind.cdp_type(),
                        "x": point.x,
                        "y": point.y,
                        "button": button.cdp_name(),
                        "clickCount": if matches!(kind, ExternalPointerKind::Press | ExternalPointerKind::Release) { 1 } else { 0 },
                        "deltaX": delta_x,
                        "deltaY": delta_y,
                        "modifiers": modifiers,
                    }),
                )
                .await?;
        }
        ExternalInputAction::Focus { focused } => {
            target
                .send(
                    "Emulation.setFocusEmulationEnabled",
                    json!({ "enabled": focused }),
                )
                .await?;
        }
    }
    Ok(())
}

fn cdp_key_metadata(key: &str) -> Option<(String, u32)> {
    let fixed = match key {
        "Enter" => ("Enter", 13),
        "Escape" => ("Escape", 27),
        "Backspace" => ("Backspace", 8),
        "Tab" => ("Tab", 9),
        "ArrowUp" => ("ArrowUp", 38),
        "ArrowDown" => ("ArrowDown", 40),
        "ArrowLeft" => ("ArrowLeft", 37),
        "ArrowRight" => ("ArrowRight", 39),
        "Home" => ("Home", 36),
        "End" => ("End", 35),
        "PageUp" => ("PageUp", 33),
        "PageDown" => ("PageDown", 34),
        "Insert" => ("Insert", 45),
        "Delete" => ("Delete", 46),
        " " => ("Space", 32),
        _ => {
            if let Some(function) = key
                .strip_prefix('F')
                .and_then(|value| value.parse::<u32>().ok())
                && (1..=24).contains(&function)
            {
                return Some((key.to_owned(), 111 + function));
            }
            let mut characters = key.chars();
            let character = characters.next()?;
            if characters.next().is_some() || !character.is_ascii_alphanumeric() {
                return None;
            }
            let uppercase = character.to_ascii_uppercase();
            let code = if uppercase.is_ascii_alphabetic() {
                format!("Key{uppercase}")
            } else {
                format!("Digit{uppercase}")
            };
            return Some((code, u32::from(uppercase)));
        }
    };
    Some((fixed.0.to_owned(), fixed.1))
}

fn cdp_activation_text(key: &str) -> Option<&'static str> {
    match key {
        "Enter" => Some("\r"),
        " " => Some(" "),
        _ => None,
    }
}

fn pointer_button_action(
    button: ExternalPointerButton,
    point: ExternalInputPoint,
    modifiers: u8,
    pressed: bool,
) -> ExternalInputAction {
    ExternalInputAction::Pointer {
        kind: if pressed {
            ExternalPointerKind::Press
        } else {
            ExternalPointerKind::Release
        },
        button,
        point,
        modifiers,
        delta_x: 0.0,
        delta_y: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter() -> Result<ExternalInputAdapter, ExternalInputError> {
        let geometry = ExternalInputGeometry::new(10, 4, 1000.0, 400.0)?;
        ExternalInputAdapter::new(geometry)
    }

    #[test]
    fn cell_centers_scale_to_css_geometry() -> Result<(), ExternalInputError> {
        let geometry = ExternalInputGeometry::new(10, 4, 1000.0, 400.0)?;
        assert_eq!(
            geometry.cell_center(2, 1)?,
            ExternalInputPoint { x: 250.0, y: 150.0 }
        );
        Ok(())
    }

    #[test]
    fn character_key_translates_to_key_and_text_actions() -> Result<(), ExternalInputError> {
        let event = InputEvent::Key {
            code: KeyCode::Character('x'),
            modifiers: Modifiers::SHIFT | Modifiers::ALT,
            kind: KeyEventKind::Repeat,
            shifted_key: Some('X'),
            base_layout_key: None,
            text: Some("X".to_owned()),
        };
        assert_eq!(
            adapter()?.translate(&event)?,
            vec![
                ExternalInputAction::Key {
                    kind: ExternalKeyEventKind::Repeat,
                    key: "X".to_owned(),
                    modifiers: 9,
                },
                ExternalInputAction::InsertText {
                    text: "X".to_owned(),
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn cdp_key_metadata_preserves_keyboard_default_actions() {
        assert_eq!(cdp_key_metadata("Enter"), Some(("Enter".to_owned(), 13)));
        assert_eq!(cdp_key_metadata(" "), Some(("Space".to_owned(), 32)));
        assert_eq!(cdp_key_metadata("X"), Some(("KeyX".to_owned(), 88)));
        assert_eq!(cdp_activation_text("Enter"), Some("\r"));
        assert_eq!(cdp_activation_text(" "), Some(" "));
        assert_eq!(cdp_activation_text("Tab"), None);
    }

    #[test]
    fn pointer_translation_preserves_button_modifiers_and_wheel_direction()
    -> Result<(), ExternalInputError> {
        let pointer = InputEvent::Mouse {
            button: MouseButton::Middle,
            column: 9,
            row: 3,
            pressed: true,
            modifiers: Modifiers::CONTROL | Modifiers::META,
        };
        assert_eq!(
            adapter()?.translate(&pointer)?,
            vec![ExternalInputAction::Pointer {
                kind: ExternalPointerKind::Press,
                button: ExternalPointerButton::Middle,
                point: ExternalInputPoint { x: 950.0, y: 350.0 },
                modifiers: 6,
                delta_x: 0.0,
                delta_y: 0.0,
            }]
        );

        let wheel = InputEvent::Mouse {
            button: MouseButton::WheelUp,
            column: 0,
            row: 0,
            pressed: true,
            modifiers: Modifiers::empty(),
        };
        assert_eq!(
            adapter()?.translate(&wheel)?,
            vec![ExternalInputAction::Pointer {
                kind: ExternalPointerKind::Wheel,
                button: ExternalPointerButton::None,
                point: ExternalInputPoint { x: 50.0, y: 50.0 },
                modifiers: 0,
                delta_x: 0.0,
                delta_y: -120.0,
            }]
        );

        let horizontal_wheel = InputEvent::Mouse {
            button: MouseButton::WheelRight,
            column: 0,
            row: 0,
            pressed: true,
            modifiers: Modifiers::empty(),
        };
        assert_eq!(
            adapter()?.translate(&horizontal_wheel)?,
            vec![ExternalInputAction::Pointer {
                kind: ExternalPointerKind::Wheel,
                button: ExternalPointerButton::None,
                point: ExternalInputPoint { x: 50.0, y: 50.0 },
                modifiers: 0,
                delta_x: 120.0,
                delta_y: 0.0,
            }]
        );

        let shifted_wheel = InputEvent::Mouse {
            button: MouseButton::WheelUp,
            column: 0,
            row: 0,
            pressed: true,
            modifiers: Modifiers::SHIFT,
        };
        assert_eq!(
            adapter()?.translate(&shifted_wheel)?,
            vec![ExternalInputAction::Pointer {
                kind: ExternalPointerKind::Wheel,
                button: ExternalPointerButton::None,
                point: ExternalInputPoint { x: 50.0, y: 50.0 },
                modifiers: 8,
                delta_x: -120.0,
                delta_y: 0.0,
            }]
        );

        let motion = InputEvent::Mouse {
            button: MouseButton::None,
            column: 2,
            row: 1,
            pressed: false,
            modifiers: Modifiers::SHIFT,
        };
        assert_eq!(
            adapter()?.translate(&motion)?,
            vec![ExternalInputAction::Pointer {
                kind: ExternalPointerKind::Move,
                button: ExternalPointerButton::None,
                point: ExternalInputPoint { x: 250.0, y: 150.0 },
                modifiers: 8,
                delta_x: 0.0,
                delta_y: 0.0,
            }]
        );
        Ok(())
    }

    #[test]
    fn validation_rejects_invalid_geometry_and_unrepresentable_input()
    -> Result<(), ExternalInputError> {
        assert!(matches!(
            ExternalInputGeometry::new(0, 1, 1.0, 1.0),
            Err(ExternalInputError::InvalidGeometry)
        ));
        assert!(matches!(
            ExternalInputGeometry::new(1, 1, f64::NAN, 1.0),
            Err(ExternalInputError::InvalidGeometry)
        ));
        let event = InputEvent::Key {
            code: KeyCode::Unidentified,
            modifiers: Modifiers::empty(),
            kind: KeyEventKind::Press,
            shifted_key: None,
            base_layout_key: None,
            text: None,
        };
        assert!(matches!(
            adapter()?.translate(&event),
            Err(ExternalInputError::UnsupportedKey)
        ));
        let outside_grid = InputEvent::Mouse {
            button: MouseButton::None,
            column: 10,
            row: 0,
            pressed: true,
            modifiers: Modifiers::empty(),
        };
        assert!(matches!(
            adapter()?.translate(&outside_grid),
            Err(ExternalInputError::TerminalCoordinate { .. })
        ));
        Ok(())
    }

    #[test]
    fn cancellation_is_checked_before_target_dispatch() -> Result<(), ExternalInputError> {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        assert!(matches!(
            ensure_active(&cancellation),
            Err(ExternalInputError::Cancelled)
        ));
        Ok(())
    }
}
