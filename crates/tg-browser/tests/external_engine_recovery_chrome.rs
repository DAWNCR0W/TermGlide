use std::error::Error;
use std::io;
use std::path::Path;

use serde_json::Value;
use tg_browser::{
    ExternalEngineError, ExternalEngineLaunchOptions, ExternalEngineProcess,
    ExternalEngineViewport, discover_external_engine, launch_external_engine,
};
use tg_core::Cancellation;
use tg_network::{CdpError, CdpLimits, CdpReply, CdpSession};
use url::Url;

type TestResult = Result<(), Box<dyn Error>>;

const FIXTURE_URL: &str = "data:text/html,%3C!doctype%20html%3E%3Cmeta%20charset=utf-8%3E%3Cmain%20id=fixture%3Erecovery%3C/main%3E";

#[tokio::test]
async fn external_engine_recovers_after_supervised_termination() -> TestResult {
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

    let mut first_process = launch_external_engine(&options, &cancellation)?;
    let first_profile = first_process.isolated_profile_dir().to_path_buf();
    let first_endpoint = first_process.endpoint().websocket_url();
    let first_preconditions = preserve_results(
        require_executable_path(&first_process, selected_executable.as_path()),
        require_profile_exists(&first_profile),
    );
    let first_flow = match first_preconditions {
        Ok(()) => {
            exercise_terminated_connection(&mut first_process, &first_profile, &first_endpoint)
                .await
        }
        Err(error) => Err(error),
    };
    let first_cleanup = shutdown_process(first_process, &first_profile);
    preserve_results(first_flow, first_cleanup)?;

    let second_process = launch_external_engine(&options, &cancellation)?;
    let second_profile = second_process.isolated_profile_dir().to_path_buf();
    let second_endpoint = second_process.endpoint().websocket_url();
    let second_preconditions = preserve_results(
        preserve_results(
            require_executable_path(&second_process, selected_executable.as_path()),
            require_profile_exists(&second_profile),
        ),
        require_distinct_profiles(&first_profile, &second_profile),
    );
    let second_flow = match second_preconditions {
        Ok(()) => exercise_recovered_connection(&second_endpoint).await,
        Err(error) => Err(error),
    };
    let second_cleanup = shutdown_process(second_process, &second_profile);
    preserve_results(second_flow, second_cleanup)
}

async fn exercise_terminated_connection(
    process: &mut ExternalEngineProcess,
    profile: &Path,
    endpoint: &str,
) -> TestResult {
    let mut connection = CdpSession::connect(endpoint, CdpLimits::default()).await?;
    let flow_result = async {
        {
            let mut target = connection.attach_first_page().await?;
            target.page_enable().await?;
            navigate_fixture(&mut target).await?;
        }

        process.shutdown()?;
        require_profile_removed(profile)?;
        require_terminal_cdp_failure(connection.send("Browser.getVersion", Value::Null).await)?;
        Ok::<(), Box<dyn Error>>(())
    }
    .await;
    let close_result = connection
        .close()
        .await
        .map_err(|error| Box::new(error) as Box<dyn Error>);
    preserve_results(flow_result, close_result)
}

async fn exercise_recovered_connection(endpoint: &str) -> TestResult {
    let mut connection = CdpSession::connect(endpoint, CdpLimits::default()).await?;
    let flow_result = async {
        let mut target = connection.attach_first_page().await?;
        target.page_enable().await?;
        navigate_fixture(&mut target).await?;
        let screenshot = target.capture_screenshot().await?;
        if screenshot.data.is_empty() {
            return Err(io::Error::other("recovered browser screenshot was empty").into());
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

async fn navigate_fixture(target: &mut tg_network::CdpTargetSession<'_>) -> TestResult {
    let fixture = Url::parse(FIXTURE_URL)?;
    let navigation = target.navigate(fixture.as_str()).await?;
    if navigation.error_text.is_some() {
        return Err(io::Error::other("generic data navigation returned an error").into());
    }
    target.wait_for_load().await?;
    Ok(())
}

fn require_terminal_cdp_failure(result: Result<CdpReply, CdpError>) -> TestResult {
    match result {
        Err(CdpError::Closed | CdpError::WebSocket(_) | CdpError::Protocol(_)) => Ok(()),
        Ok(_) => {
            Err(io::Error::other("CDP accepted an operation after process termination").into())
        }
        Err(error) => Err(io::Error::other(format!(
            "CDP returned an unexpected failure after process termination: {error}"
        ))
        .into()),
    }
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

fn require_distinct_profiles(first: &Path, second: &Path) -> TestResult {
    if first != second {
        Ok(())
    } else {
        Err(io::Error::other("recovery launch reused the terminated process profile").into())
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
