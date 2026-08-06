//! Reusable application surfaces for TermGlide hosts.

pub mod external;
pub mod external_interactive;
pub mod interactive;

pub use external_interactive::{
    ExternalInteractiveCleanupError, ExternalInteractiveError, ExternalInteractiveExitReason,
    ExternalInteractiveLimits, ExternalInteractiveReport, ExternalInteractiveUnsupportedEvent,
    run_external_interactive, run_external_interactive_terminal,
};
