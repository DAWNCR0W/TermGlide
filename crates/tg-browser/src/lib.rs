//! Supervised local Chrome/Chromium lifecycle and terminal-input adapters.

mod external_engine;
mod external_input;

use tg_terminal::InputEvent;

pub use external_engine::{
    DevToolsEndpoint, ExternalEngineError, ExternalEngineImagePolicy,
    ExternalEngineJavaScriptPolicy, ExternalEngineLaunchOptions, ExternalEngineProcess,
    ExternalEngineProxy, ExternalEngineProxyBypass, ExternalEngineViewport,
    discover_external_engine, launch_external_engine,
};
pub use external_input::{
    ExternalInputAction, ExternalInputAdapter, ExternalInputDispatch, ExternalInputError,
    ExternalInputGeometry, ExternalInputLimits, ExternalInputPoint, ExternalKeyEventKind,
    ExternalPointerButton, ExternalPointerKind,
};

/// IME events are retained as an explicit unsupported boundary for terminal hosts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImeCompositionEvent {
    Start,
    Update(String),
    Commit,
    Cancel,
}

/// Decoded input accepted by the Chrome terminal loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellInputEvent {
    Terminal(InputEvent),
    Ime(ImeCompositionEvent),
}
