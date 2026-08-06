use std::error::Error;

use serde_json::Value;
use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineViewport, ExternalInputAdapter,
    ExternalInputGeometry, ExternalInputLimits, discover_external_engine, launch_external_engine,
};
use tg_core::Cancellation;
use tg_network::{CdpLimits, CdpSession};
use tg_terminal::{InputEvent, Modifiers, MouseButton};
use url::Url;

const FIXTURE_URL: &str = "data:text/html,%3C!doctype%20html%3E%3Cmeta%20charset=utf-8%3E%3Cstyle%3Ebody%7Bmargin:0;padding:16px%7Dinput%7Bdisplay:block;width:160px;height:28px%7Dbutton%7Bdisplay:block;margin-top:8px;width:100px;height:32px%7D%3C/style%3E%3Cinput%20id=entry%20aria-label=entry%3E%3Cbutton%20id=commit%20type=button%3Ecommit%3C/button%3E%3Coutput%20id=state%3Eidle%3C/output%3E%3Cscript%3Edocument.getElementById(%22commit%22).addEventListener(%22click%22,()%3D%3E%7Bdocument.getElementById(%22state%22).textContent%3Ddocument.getElementById(%22entry%22).value%7D)%3C/script%3E";

#[tokio::test]
async fn external_input_adapter_dispatches_to_isolated_chrome() -> Result<(), Box<dyn Error>> {
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

    let flow_result = exercise_input(&endpoint).await;
    let liveness = process.try_wait();
    let shutdown = process.shutdown();
    drop(process);

    assert!(!profile_dir.exists());
    flow_result?;
    assert!(liveness?.is_none());
    shutdown?;
    Ok(())
}

async fn exercise_input(endpoint: &str) -> Result<(), Box<dyn Error>> {
    let mut connection = CdpSession::connect(endpoint, CdpLimits::default()).await?;
    let flow_result = async {
        let mut target = connection.attach_first_page().await?;
        assert!(!target.session_id().is_empty());
        target.page_enable().await?;
        target.set_device_metrics(640, 480, 1.0).await?;

        let fixture = Url::parse(FIXTURE_URL)?;
        let navigation = target.navigate(fixture.as_str()).await?;
        assert!(navigation.error_text.is_none());
        target.wait_for_load().await?;

        let adapter = ExternalInputAdapter::with_limits(
            ExternalInputGeometry::new(80, 24, 640.0, 480.0)?,
            ExternalInputLimits::BROWSER_DEFAULT,
        )?;
        let input_cancellation = Cancellation::new();

        let focus_dispatch = adapter
            .dispatch(&mut target, &InputEvent::Focus(true), &input_cancellation)
            .await?;
        assert_eq!(focus_dispatch.sent_actions(), 1);

        for pressed in [true, false] {
            let click_dispatch = adapter
                .dispatch(
                    &mut target,
                    &left_mouse_event(8, 1, pressed),
                    &input_cancellation,
                )
                .await?;
            assert_eq!(click_dispatch.sent_actions(), 1);
        }

        let text_dispatch = adapter
            .dispatch(
                &mut target,
                &InputEvent::Paste("adapter".to_owned()),
                &input_cancellation,
            )
            .await?;
        assert_eq!(text_dispatch.sent_actions(), 1);

        let input_state = target
            .runtime_evaluate(
                "({focused:document.activeElement===document.getElementById('entry'),value:document.getElementById('entry').value})",
                true,
            )
            .await?;
        assert!(input_state.exception_details.is_none());
        assert_eq!(
            input_state
                .result
                .pointer("/value/focused")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            input_state
                .result
                .pointer("/value/value")
                .and_then(Value::as_str),
            Some("adapter")
        );

        for pressed in [true, false] {
            let click_dispatch = adapter
                .dispatch(
                    &mut target,
                    &left_mouse_event(8, 3, pressed),
                    &input_cancellation,
                )
                .await?;
            assert_eq!(click_dispatch.sent_actions(), 1);
        }

        let click_state = target
            .runtime_evaluate("document.getElementById('state').textContent", true)
            .await?;
        assert!(click_state.exception_details.is_none());
        assert_eq!(
            click_state.result.get("value").and_then(Value::as_str),
            Some("adapter")
        );

        let screenshot = target.capture_screenshot().await?;
        assert!(!screenshot.data.is_empty());
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

fn left_mouse_event(column: u16, row: u16, pressed: bool) -> InputEvent {
    InputEvent::Mouse {
        button: MouseButton::Left,
        column,
        row,
        pressed,
        modifiers: Modifiers::empty(),
    }
}
