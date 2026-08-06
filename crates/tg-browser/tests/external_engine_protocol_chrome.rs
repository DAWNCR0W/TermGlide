use std::error::Error;
use std::io;
use std::path::Path;

use serde_json::Value;
use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineProcess,
    ExternalEngineViewport, discover_external_engine, launch_external_engine,
};
use tg_core::Cancellation;
use tg_network::{CdpBrowserVersion, CdpLimits, CdpSession};
use url::Url;

type TestResult = Result<(), Box<dyn Error>>;

const FIXTURE_URL: &str = "data:text/html,%3C!doctype%20html%3E%3Cmeta%20charset=utf-8%3E%3Cmain%20id=fixture%3Eprotocol%20journey%3C/main%3E";

#[tokio::test]
async fn external_engine_reports_bounded_protocol_metadata() -> TestResult {
    let selected_executable = match discover_external_engine(None) {
        Ok(path) => path,
        Err(ExternalEngineError::ExecutableNotFound) => return Ok(()),
        Err(error) => return Err(Box::new(error) as Box<dyn Error>),
    };
    if !selected_executable.is_file() {
        return Err(io::Error::other("discovered executable is not a regular file").into());
    }

    let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
    options.executable_override = Some(selected_executable.clone());
    options.viewport = Some(ExternalEngineViewport::new(640, 480)?);

    let cancellation = Cancellation::new();
    let mut process = launch_external_engine(&options, &cancellation)?;
    let profile = process.isolated_profile_dir().to_path_buf();
    let endpoint = process.endpoint().websocket_url();
    let preconditions = preserve_results(
        require_executable_path(&process, selected_executable.as_path()),
        require_profile_exists(&profile),
    );
    let flow = match preconditions {
        Ok(()) => exercise_protocol_journey(&endpoint).await,
        Err(error) => Err(error),
    };
    let liveness = require_process_running(&mut process);
    let cleanup = shutdown_process(process, &profile);
    preserve_results(preserve_results(flow, liveness), cleanup)
}

async fn exercise_protocol_journey(endpoint: &str) -> TestResult {
    let limits = CdpLimits::default();
    let max_metadata_bytes = limits.max_message_bytes;
    let mut connection = CdpSession::connect(endpoint, limits).await?;
    let flow_result = async {
        let version = connection.browser_version().await?;
        require_bounded_version_metadata(&version, max_metadata_bytes)?;

        let mut target = connection.attach_first_page().await?;
        target.page_enable().await?;
        let fixture = Url::parse(FIXTURE_URL)?;
        let navigation = target.navigate(fixture.as_str()).await?;
        if navigation.error_text.is_some() {
            return Err(io::Error::other("generic data navigation returned an error").into());
        }
        target.wait_for_load().await?;

        let fixture_state = target
            .runtime_evaluate("document.getElementById('fixture')?.textContent", true)
            .await?;
        if fixture_state.exception_details.is_some() {
            return Err(io::Error::other("fixture evaluation returned an exception").into());
        }
        if fixture_state.result.get("value").and_then(Value::as_str) != Some("protocol journey") {
            return Err(
                io::Error::other("data fixture did not produce the expected document").into(),
            );
        }

        Ok::<(), Box<dyn Error>>(())
    }
    .await;
    let close_result = connection
        .close()
        .await
        .map_err(|error| Box::new(error) as Box<dyn Error>);
    preserve_results(flow_result, close_result)
}

fn require_bounded_version_metadata(
    version: &CdpBrowserVersion,
    max_metadata_bytes: usize,
) -> TestResult {
    for (field, value) in [
        ("product", version.product.as_str()),
        ("protocolVersion", version.protocol_version.as_str()),
        ("revision", version.revision.as_str()),
        ("userAgent", version.user_agent.as_str()),
        ("jsVersion", version.js_version.as_str()),
    ] {
        if value.is_empty() {
            return Err(io::Error::other(format!("Browser.getVersion {field} was empty")).into());
        }
        if value.len() > max_metadata_bytes {
            return Err(io::Error::other(format!(
                "Browser.getVersion {field} exceeded the configured byte limit"
            ))
            .into());
        }
        if value.chars().any(char::is_control) {
            return Err(io::Error::other(format!(
                "Browser.getVersion {field} contained a control character"
            ))
            .into());
        }
    }
    Ok(())
}

fn require_executable_path(process: &ExternalEngineProcess, expected: &Path) -> TestResult {
    if process.executable_path() == expected {
        Ok(())
    } else {
        Err(
            io::Error::other("process executable provenance did not match the selected path")
                .into(),
        )
    }
}

fn require_profile_exists(profile: &Path) -> TestResult {
    if profile.is_dir() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "isolated profile was missing before cleanup: {}",
            profile.display()
        ))
        .into())
    }
}

fn require_process_running(process: &mut ExternalEngineProcess) -> TestResult {
    match process.try_wait()? {
        None => Ok(()),
        Some(status) => Err(io::Error::other(format!(
            "external engine exited before supervised shutdown: {status:?}"
        ))
        .into()),
    }
}

fn require_profile_removed(profile: &Path) -> TestResult {
    if !profile.exists() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "isolated profile remained after cleanup: {}",
            profile.display()
        ))
        .into())
    }
}

fn shutdown_process(mut process: ExternalEngineProcess, profile: &Path) -> TestResult {
    let shutdown_result = process
        .shutdown()
        .map_err(|error| Box::new(error) as Box<dyn Error>);
    drop(process);
    preserve_results(shutdown_result, require_profile_removed(profile))
}

fn preserve_results(primary: TestResult, cleanup: TestResult) -> TestResult {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(primary), Err(cleanup)) => Err(io::Error::other(format!(
            "primary failure: {primary}; cleanup failure: {cleanup}"
        ))
        .into()),
    }
}
