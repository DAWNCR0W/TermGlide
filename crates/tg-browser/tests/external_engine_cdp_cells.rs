use std::error::Error;

use serde_json::Value;
use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineViewport,
    discover_external_engine, launch_external_engine,
};
use tg_core::{Cancellation, SystemClock};
use tg_network::{CdpError, CdpLimits, CdpSession};
use tg_terminal::{
    Backend, ExternalFrameDecodeLimits, ExternalFrameError, ExternalFrameOptions,
    ExternalFrameSequence, ExternalFrameSequenceLimits, ExternalFrameSequenceOutput,
    ProjectionLimits, TerminalOp, TerminalTransactionControl, TerminalTransactionError,
};
use url::Url;

const FIXTURE_URL: &str = "data:text/html,%3C!doctype%20html%3E%3Cmeta%20charset=utf-8%3E%3Cstyle%3Ebody%7Bmargin:0;padding:16px%7Dinput%7Bdisplay:block;width:160px;height:28px%7Dbutton%7Bdisplay:block;margin-top:8px;width:100px;height:32px%7D%3C/style%3E%3Cinput%20id=entry%20aria-label=entry%3E%3Cbutton%20id=commit%20type=button%3Ecommit%3C/button%3E%3Coutput%20id=state%3Eidle%3C/output%3E%3Cscript%3Edocument.getElementById(%22commit%22).addEventListener(%22click%22,()%3D%3E%7Bdocument.getElementById(%22state%22).textContent%3Ddocument.getElementById(%22entry%22).value%7D)%3C/script%3E";

#[tokio::test]
async fn external_engine_cdp_fixture_projects_cells_and_removes_profile()
-> Result<(), Box<dyn Error>> {
    let selected_executable = match discover_external_engine(None) {
        Ok(path) => path,
        Err(error @ ExternalEngineError::ExecutableNotFound) => {
            assert!(matches!(error, ExternalEngineError::ExecutableNotFound));
            return Ok(());
        }
        Err(error) => return Err(Box::new(error) as Box<dyn Error>),
    };
    assert!(selected_executable.is_file());

    let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
    options.executable_override = Some(selected_executable.clone());
    options.viewport = Some(ExternalEngineViewport::new(640, 480)?);

    let cancellation = Cancellation::new();
    let mut process = launch_external_engine(&options, &cancellation)?;
    assert_eq!(process.executable_path(), selected_executable.as_path());
    let profile_dir = process.isolated_profile_dir().to_path_buf();
    assert!(profile_dir.is_dir());
    let endpoint = process.endpoint().websocket_url();

    let flow_result = exercise_fixture(&endpoint).await;
    let liveness = process.try_wait();
    let shutdown = process.shutdown();
    drop(process);

    assert!(!profile_dir.exists());
    flow_result?;
    assert!(liveness?.is_none());
    shutdown?;
    Ok(())
}

async fn exercise_fixture(endpoint: &str) -> Result<(), Box<dyn Error>> {
    let mut connection = CdpSession::connect(endpoint, CdpLimits::default()).await?;
    let flow_result = async {
        let browser_version = connection.browser_version().await?;
        assert!(!browser_version.protocol_version.is_empty());
        assert!(!browser_version.product.is_empty());

        let mut target = connection.attach_first_page().await?;
        target.page_enable().await?;

        let fixture = Url::parse(FIXTURE_URL)?;
        let navigation = target.navigate(fixture.as_str()).await?;
        assert!(navigation.error_text.is_none());
        target.wait_for_load().await?;

        let readiness = target
            .runtime_evaluate("document.getElementById('entry') !== null", true)
            .await?;
        assert!(readiness.exception_details.is_none());
        assert_eq!(
            readiness.result.get("value").and_then(Value::as_bool),
            Some(true)
        );

        let focused = target
            .runtime_evaluate("document.getElementById('entry').focus()", false)
            .await?;
        assert!(focused.exception_details.is_none());
        target.key_down("Shift").await?;
        target.key_up("Shift").await?;
        target.insert_text("termglide").await?;
        target.mouse_click(66.0, 68.0).await?;

        let state = target
            .runtime_evaluate(
                "({value:document.getElementById('entry').value,state:document.getElementById('state').textContent})",
                true,
            )
            .await?;
        assert!(state.exception_details.is_none());
        assert_eq!(
            state
                .result
                .pointer("/value/value")
                .and_then(Value::as_str),
            Some("termglide")
        );
        assert_eq!(
            state
                .result
                .pointer("/value/state")
                .and_then(Value::as_str),
            Some("termglide")
        );

        let cancellation = Cancellation::new();
        let clock = SystemClock::default();
        let control = TerminalTransactionControl::new(&cancellation, &clock, None);
        let mut sequence = ExternalFrameSequence::new();

        let first_screenshot = target.capture_screenshot().await?;
        let first = project_cells(&mut sequence, &first_screenshot.data, &control)?;
        assert!(!first.is_identical());
        assert!(!first.operations().is_empty());
        assert!(!first.output().is_empty());
        assert!(sequence.previous().is_some());

        let mutation = target
            .runtime_evaluate(
                r#"(() => {
                    const marker = document.createElement("div");
                    marker.style.cssText = "position:fixed;inset:0;background:#0033cc";
                    document.body.append(marker);
                })()"#,
                false,
            )
            .await?;
        assert!(mutation.exception_details.is_none());

        let second_screenshot = target.capture_screenshot().await?;
        let changed = project_cells(&mut sequence, &second_screenshot.data, &control)?;
        assert!(!changed.is_identical());
        assert!(!changed.operations().is_empty());
        assert!(
            !changed
                .operations()
                .iter()
                .any(|operation| matches!(operation, TerminalOp::ClearScreen))
        );
        assert!(!changed.output().is_empty());

        let third_screenshot = target.capture_screenshot().await?;
        let identical = project_cells(&mut sequence, &third_screenshot.data, &control)?;
        assert!(identical.is_identical());
        assert!(identical.operations().is_empty());
        assert!(identical.output().is_empty());
        assert!(sequence.previous().is_some());

        let cancelled = Cancellation::new();
        cancelled.cancel();
        let cancelled_control = TerminalTransactionControl::new(&cancelled, &clock, None);
        assert!(matches!(
            sequence.reset_controlled(&cancelled_control),
            Err(ExternalFrameError::Transaction(
                TerminalTransactionError::Cancelled
            ))
        ));
        assert!(sequence.previous().is_some());

        let protocol_failure = target.send("TermGlide.integrationProbe", Value::Null).await;
        assert!(matches!(protocol_failure, Err(CdpError::Protocol(_))));
        Ok::<(), Box<dyn Error>>(())
    }
    .await;
    let close_result = connection.close().await;

    match (flow_result, close_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(Box::new(error)),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn project_cells(
    sequence: &mut ExternalFrameSequence,
    screenshot: &str,
    control: &TerminalTransactionControl<'_>,
) -> Result<ExternalFrameSequenceOutput, ExternalFrameError> {
    sequence.push_png_base64_controlled(
        screenshot,
        ExternalFrameOptions::new(
            Backend::Cells,
            80,
            24,
            ExternalFrameDecodeLimits::BROWSER_DEFAULT,
            ProjectionLimits::default(),
            ExternalFrameSequenceLimits::BROWSER_DEFAULT,
        ),
        control,
    )
}
