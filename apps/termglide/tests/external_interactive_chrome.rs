use std::collections::HashMap;
use std::error::Error;
use std::io;

use futures_util::stream;
use termglide::external::{
    ExternalEnginePolicies, ExternalRenderLimits, ExternalRenderRequest, ExternalViewport,
};
use termglide::interactive::{InteractiveError, InteractiveEvent};
use termglide::{
    ExternalInteractiveError, ExternalInteractiveExitReason, ExternalInteractiveLimits,
    run_external_interactive,
};
use tg_browser::{ExternalEngineError, ShellInputEvent, discover_external_engine};
use tg_core::Cancellation;
use tg_terminal::{Backend, InputEvent, TerminalCapabilities};

type TestResult = Result<(), Box<dyn Error>>;

const EVENT_COUNT: usize = 3;
const MIN_EXPECTED_FRAME_WRITES: usize = 2;
const MAX_FRAME_ANSI_BYTES: usize = 128 * 1024;

const FIXTURE_DOCUMENT: &str = r#"<!doctype html>
<meta charset="utf-8">
<style>
html,body{margin:0;width:100%;height:100%;background:#000;color:#fff;font-family:monospace}
#field{position:absolute;left:16px;top:16px;width:70%;height:56px;font-size:32px}
#state{box-sizing:border-box;width:100%;height:100%;padding:112px 24px;background:#000;color:#fff;font-size:56px}
.changed #state{background:#fff;color:#000}
</style>
<input id="field" autofocus>
<div id="state">before</div>
<script>
const field=document.getElementById("field");
field.focus();
field.addEventListener("input",()=>{document.body.className="changed";document.getElementById("state").textContent="changed";});
</script>"#;

#[tokio::test]
async fn external_interactive_renders_input_mutation_and_resize() -> TestResult {
    let executable = match discover_external_engine(None) {
        Ok(path) => path,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => return Err(Box::new(error) as Box<dyn Error>),
    };
    if !executable.is_file() {
        return Err(io::Error::other("discovered executable is not a regular file").into());
    }

    let request = ExternalRenderRequest {
        target: fixture_target(),
        policies: ExternalEnginePolicies {
            executable_override: Some(executable),
            ..ExternalEnginePolicies::default()
        },
        viewport: ExternalViewport {
            width: 640,
            height: 360,
            device_scale: 1.0,
        },
        terminal_columns: 64,
        terminal_rows: 18,
        backend: Backend::Cells,
        capabilities: supported_terminal_capabilities(),
        limits: ExternalRenderLimits {
            max_bytes: MAX_FRAME_ANSI_BYTES,
            max_cells: 4_096,
        },
    };
    let events = stream::iter(vec![
        Ok::<InteractiveEvent, InteractiveError>(InteractiveEvent::Input(
            ShellInputEvent::Terminal(InputEvent::Paste("mutate".to_owned())),
        )),
        Ok(InteractiveEvent::Resize {
            columns: 80,
            rows: 24,
        }),
        Ok(InteractiveEvent::Quit),
    ]);
    let cancellation = Cancellation::new();
    let limits = ExternalInteractiveLimits {
        max_events: EVENT_COUNT,
        ..ExternalInteractiveLimits::default()
    };
    let mut ansi = Vec::new();

    let report = preserve_operation_cleanup_diagnostics(
        run_external_interactive(request, events, &mut ansi, &cancellation, limits).await,
    )?;

    if report.reason != ExternalInteractiveExitReason::Quit {
        return Err(io::Error::other(format!(
            "interactive runner exited for {:?}, not Quit",
            report.reason
        ))
        .into());
    }
    if report.events_processed != EVENT_COUNT {
        return Err(io::Error::other(format!(
            "interactive runner processed {} events, expected {EVENT_COUNT}",
            report.events_processed
        ))
        .into());
    }
    if report.frames_written < MIN_EXPECTED_FRAME_WRITES {
        return Err(io::Error::other(format!(
            "interactive runner wrote {} frames; expected at least {MIN_EXPECTED_FRAME_WRITES} distinct initial and input-mutated/resized frames",
            report.frames_written,
        ))
        .into());
    }
    if report.relaunches != 0 {
        return Err(io::Error::other(format!(
            "interactive runner unexpectedly relaunched {} times",
            report.relaunches
        ))
        .into());
    }
    if ansi.is_empty() {
        return Err(io::Error::other("interactive runner produced no ANSI output").into());
    }
    let total_ansi_limit = MAX_FRAME_ANSI_BYTES
        .checked_mul(report.frames_written)
        .ok_or_else(|| io::Error::other("ANSI frame bound overflowed"))?;
    if ansi.len() > total_ansi_limit {
        return Err(io::Error::other(format!(
            "interactive ANSI output used {} bytes, exceeding {total_ansi_limit}",
            ansi.len()
        ))
        .into());
    }

    Ok(())
}

fn fixture_target() -> String {
    format!(
        "data:text/html,{}",
        url::form_urlencoded::byte_serialize(FIXTURE_DOCUMENT.as_bytes()).collect::<String>()
    )
}

fn supported_terminal_capabilities() -> TerminalCapabilities {
    let environment = HashMap::from([
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("COLORTERM".to_owned(), "truecolor".to_owned()),
    ]);
    TerminalCapabilities::from_environment(&environment)
}

fn preserve_operation_cleanup_diagnostics<T>(
    result: Result<T, ExternalInteractiveError>,
) -> Result<T, Box<dyn Error>> {
    result.map_err(|error| Box::new(error) as Box<dyn Error>)
}
