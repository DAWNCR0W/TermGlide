//! Crossterm event decoding for the supervised Chrome terminal loop.

use std::io;

use crossterm::event::{
    Event, EventStream, KeyCode as CrosstermKeyCode, KeyEvent as CrosstermKeyEvent,
    KeyEventKind as CrosstermKeyEventKind, KeyEventState, KeyModifiers,
    MouseButton as CrosstermMouseButton, MouseEvent as CrosstermMouseEvent,
    MouseEventKind as CrosstermMouseEventKind,
};
use futures_util::{Stream, StreamExt};
use tg_browser::ShellInputEvent;
use tg_terminal::{InputEvent, KeyCode, KeyEventKind, Modifiers, MouseButton};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedInputFeature {
    PointerCapture,
    HorizontalWheel,
    ExtendedKey,
    NullKey,
}

#[derive(Debug, Error)]
pub enum InteractiveError {
    #[error("interactive viewport dimensions are invalid")]
    InvalidViewport,
    #[error("interactive terminal I/O failed: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InteractiveEvent {
    Input(ShellInputEvent),
    Resize { columns: u16, rows: u16 },
    Unsupported(UnsupportedInputFeature),
    Quit,
}

pub(crate) fn external_crossterm_events()
-> impl Stream<Item = Result<InteractiveEvent, InteractiveError>> + Unpin {
    EventStream::new().map(|event| match event {
        Ok(event) => translate_external_crossterm_event(event),
        Err(error) => Err(InteractiveError::Io(error)),
    })
}

pub fn translate_crossterm_event(event: Event) -> Result<InteractiveEvent, InteractiveError> {
    translate_crossterm_event_with_external_input(event, false)
}

fn translate_external_crossterm_event(event: Event) -> Result<InteractiveEvent, InteractiveError> {
    translate_crossterm_event_with_external_input(event, true)
}

fn translate_crossterm_event_with_external_input(
    event: Event,
    external_input: bool,
) -> Result<InteractiveEvent, InteractiveError> {
    match event {
        Event::FocusGained => Ok(terminal_event(InputEvent::Focus(true))),
        Event::FocusLost => Ok(terminal_event(InputEvent::Focus(false))),
        Event::Paste(text) => Ok(terminal_event(InputEvent::Paste(text))),
        Event::Resize(columns, rows) if columns > 0 && rows > 0 => {
            Ok(InteractiveEvent::Resize { columns, rows })
        }
        Event::Resize(_, _) => Err(InteractiveError::InvalidViewport),
        Event::Key(event) if is_quit_key(event) => Ok(InteractiveEvent::Quit),
        Event::Key(event) => translate_key(event),
        Event::Mouse(event) => translate_mouse(event, external_input),
    }
}

fn is_quit_key(event: CrosstermKeyEvent) -> bool {
    matches!(event.kind, CrosstermKeyEventKind::Press)
        && matches!(event.code, CrosstermKeyCode::Char('c' | 'C'))
        && event.modifiers.contains(KeyModifiers::CONTROL)
}

fn translate_key(event: CrosstermKeyEvent) -> Result<InteractiveEvent, InteractiveError> {
    let mut modifiers = translate_modifiers(event.modifiers, event.state);
    let code = match event.code {
        CrosstermKeyCode::Backspace => KeyCode::Backspace,
        CrosstermKeyCode::Enter => KeyCode::Enter,
        CrosstermKeyCode::Left => KeyCode::Left,
        CrosstermKeyCode::Right => KeyCode::Right,
        CrosstermKeyCode::Up => KeyCode::Up,
        CrosstermKeyCode::Down => KeyCode::Down,
        CrosstermKeyCode::Home => KeyCode::Home,
        CrosstermKeyCode::End => KeyCode::End,
        CrosstermKeyCode::PageUp => KeyCode::PageUp,
        CrosstermKeyCode::PageDown => KeyCode::PageDown,
        CrosstermKeyCode::Tab => KeyCode::Tab,
        CrosstermKeyCode::BackTab => {
            modifiers.insert(Modifiers::SHIFT);
            KeyCode::Tab
        }
        CrosstermKeyCode::Delete => KeyCode::Delete,
        CrosstermKeyCode::Insert => KeyCode::Insert,
        CrosstermKeyCode::F(number) if number > 0 => KeyCode::Function(number),
        CrosstermKeyCode::F(_) => {
            return Ok(InteractiveEvent::Unsupported(
                UnsupportedInputFeature::ExtendedKey,
            ));
        }
        CrosstermKeyCode::Char(character) if !character.is_control() => {
            KeyCode::Character(character)
        }
        CrosstermKeyCode::Char(_) | CrosstermKeyCode::Null => {
            return Ok(InteractiveEvent::Unsupported(
                UnsupportedInputFeature::NullKey,
            ));
        }
        CrosstermKeyCode::Esc => KeyCode::Escape,
        CrosstermKeyCode::CapsLock => KeyCode::Functional(1),
        CrosstermKeyCode::ScrollLock => KeyCode::Functional(2),
        CrosstermKeyCode::NumLock => KeyCode::Functional(3),
        CrosstermKeyCode::PrintScreen => KeyCode::Functional(4),
        CrosstermKeyCode::Pause => KeyCode::Functional(5),
        CrosstermKeyCode::Menu => KeyCode::Functional(6),
        CrosstermKeyCode::KeypadBegin => KeyCode::Functional(7),
        CrosstermKeyCode::Media(_) | CrosstermKeyCode::Modifier(_) => {
            return Ok(InteractiveEvent::Unsupported(
                UnsupportedInputFeature::ExtendedKey,
            ));
        }
    };
    let text = match code {
        KeyCode::Character(character)
            if !modifiers.intersects(
                Modifiers::CONTROL
                    | Modifiers::ALT
                    | Modifiers::SUPER
                    | Modifiers::HYPER
                    | Modifiers::META,
            ) =>
        {
            Some(character.to_string())
        }
        _ => None,
    };
    let kind = match event.kind {
        CrosstermKeyEventKind::Press => KeyEventKind::Press,
        CrosstermKeyEventKind::Repeat => KeyEventKind::Repeat,
        CrosstermKeyEventKind::Release => KeyEventKind::Release,
    };
    Ok(terminal_event(InputEvent::Key {
        code,
        modifiers,
        kind,
        shifted_key: None,
        base_layout_key: None,
        text,
    }))
}

fn translate_modifiers(modifiers: KeyModifiers, state: KeyEventState) -> Modifiers {
    let mut translated = Modifiers::empty();
    for (source, target) in [
        (KeyModifiers::SHIFT, Modifiers::SHIFT),
        (KeyModifiers::ALT, Modifiers::ALT),
        (KeyModifiers::CONTROL, Modifiers::CONTROL),
        (KeyModifiers::SUPER, Modifiers::SUPER),
        (KeyModifiers::HYPER, Modifiers::HYPER),
        (KeyModifiers::META, Modifiers::META),
    ] {
        if modifiers.contains(source) {
            translated.insert(target);
        }
    }
    if state.contains(KeyEventState::CAPS_LOCK) {
        translated.insert(Modifiers::CAPS_LOCK);
    }
    if state.contains(KeyEventState::NUM_LOCK) {
        translated.insert(Modifiers::NUM_LOCK);
    }
    translated
}

fn translate_mouse(
    event: CrosstermMouseEvent,
    external_input: bool,
) -> Result<InteractiveEvent, InteractiveError> {
    let modifiers = translate_modifiers(event.modifiers, KeyEventState::empty());
    let (button, pressed) = match event.kind {
        CrosstermMouseEventKind::Down(button) => (translate_mouse_button(button), true),
        CrosstermMouseEventKind::Up(button) => (translate_mouse_button(button), false),
        CrosstermMouseEventKind::ScrollDown => (MouseButton::WheelDown, true),
        CrosstermMouseEventKind::ScrollUp => (MouseButton::WheelUp, true),
        CrosstermMouseEventKind::Drag(_) if external_input => (MouseButton::None, false),
        CrosstermMouseEventKind::Drag(_) => {
            return Ok(InteractiveEvent::Unsupported(
                UnsupportedInputFeature::PointerCapture,
            ));
        }
        CrosstermMouseEventKind::Moved => (MouseButton::None, false),
        CrosstermMouseEventKind::ScrollLeft if external_input => (MouseButton::WheelLeft, true),
        CrosstermMouseEventKind::ScrollRight if external_input => (MouseButton::WheelRight, true),
        CrosstermMouseEventKind::ScrollLeft | CrosstermMouseEventKind::ScrollRight => {
            return Ok(InteractiveEvent::Unsupported(
                UnsupportedInputFeature::HorizontalWheel,
            ));
        }
    };
    Ok(terminal_event(InputEvent::Mouse {
        button,
        column: event.column,
        row: event.row,
        pressed,
        modifiers,
    }))
}

const fn translate_mouse_button(button: CrosstermMouseButton) -> MouseButton {
    match button {
        CrosstermMouseButton::Left => MouseButton::Left,
        CrosstermMouseButton::Right => MouseButton::Right,
        CrosstermMouseButton::Middle => MouseButton::Middle,
    }
}

fn terminal_event(event: InputEvent) -> InteractiveEvent {
    InteractiveEvent::Input(ShellInputEvent::Terminal(event))
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use crossterm::event::{
        Event, KeyCode as CrosstermKeyCode, KeyEvent, KeyModifiers, MouseButton as CrosstermButton,
        MouseEvent, MouseEventKind,
    };
    use tg_browser::ShellInputEvent;
    use tg_terminal::{InputEvent, MouseButton};

    use super::{
        InteractiveEvent, UnsupportedInputFeature, translate_crossterm_event,
        translate_external_crossterm_event,
    };

    #[test]
    fn control_c_quits() -> Result<(), Box<dyn Error>> {
        let event = KeyEvent::new(CrosstermKeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(
            translate_crossterm_event(Event::Key(event))?,
            InteractiveEvent::Quit
        );
        Ok(())
    }

    #[test]
    fn zero_sized_resize_is_rejected() {
        assert!(translate_crossterm_event(Event::Resize(0, 24)).is_err());
    }

    #[test]
    fn external_horizontal_wheel_is_forwarded() -> Result<(), Box<dyn Error>> {
        let event = MouseEvent {
            kind: MouseEventKind::ScrollRight,
            column: 5,
            row: 6,
            modifiers: KeyModifiers::empty(),
        };
        assert!(matches!(
            translate_crossterm_event(Event::Mouse(event)),
            Ok(InteractiveEvent::Unsupported(
                UnsupportedInputFeature::HorizontalWheel
            ))
        ));
        let translated = translate_external_crossterm_event(Event::Mouse(event))?;
        assert!(matches!(
            translated,
            InteractiveEvent::Input(ShellInputEvent::Terminal(InputEvent::Mouse {
                button: MouseButton::WheelRight,
                ..
            }))
        ));
        Ok(())
    }

    #[test]
    fn external_drag_degrades_to_pointer_motion() -> Result<(), Box<dyn Error>> {
        let translated = translate_external_crossterm_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Drag(CrosstermButton::Left),
            column: 3,
            row: 4,
            modifiers: KeyModifiers::empty(),
        }))?;
        assert!(matches!(
            translated,
            InteractiveEvent::Input(ShellInputEvent::Terminal(InputEvent::Mouse {
                button: MouseButton::None,
                pressed: false,
                ..
            }))
        ));
        Ok(())
    }
}
