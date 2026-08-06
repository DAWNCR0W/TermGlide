use std::collections::HashMap;
use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::sync::Mutex;

use clap::{Parser, Subcommand, ValueEnum};
use serde::Serialize;
use termglide::{external, external_interactive};
use tg_config::{Config, ConfigError};
use tg_core::{Cancellation, ErrorKind, ResourceLimits};
use tg_platform::{PlatformError, default_config_path, install_panic_restore_hook};
use tg_terminal::{Backend, ColorLevel, TerminalCapabilities};
use tg_url::{AddressInput, BrowserUrl, SearchEngine, UrlError, classify_address};
use thiserror::Error;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

#[derive(Debug, Parser)]
#[command(
    name = "termglide",
    version,
    about = "A graphical web browser for the terminal.",
    long_about = None
)]
struct Cli {
    #[arg(global = true, long)]
    browser_executable: Option<PathBuf>,
    #[arg(global = true, long, value_enum, default_value = "auto")]
    renderer: RendererChoice,
    #[arg(global = true, long, value_enum, default_value = "auto")]
    color: ColorChoice,
    #[arg(global = true, long)]
    viewport: Option<ViewportArgument>,
    #[arg(global = true, long)]
    scale: Option<f32>,
    #[arg(global = true, long)]
    config: Option<PathBuf>,
    #[arg(global = true, long)]
    proxy: Option<String>,
    #[arg(global = true, long, value_delimiter = ',')]
    no_proxy: Vec<String>,
    #[arg(global = true, long)]
    user_agent: Option<String>,
    #[arg(global = true, long)]
    disable_images: bool,
    #[arg(global = true, long)]
    disable_javascript: bool,
    #[arg(global = true, long)]
    data_limit: Option<ByteSize>,
    #[arg(global = true, long)]
    trace: Option<PathBuf>,
    #[arg(global = true, long, value_enum, default_value = "warn")]
    log_level: LogLevel,
    #[command(subcommand)]
    command: Option<Command>,
    url_or_query: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Inspect {
        url_or_query: String,
    },
    Snapshot {
        #[arg(long, value_enum, default_value = "cells")]
        format: SnapshotFormat,
        #[arg(long)]
        output: PathBuf,
        url_or_query: String,
    },
    Dump {
        #[arg(long, value_enum, default_value = "dom")]
        format: DumpFormat,
        url_or_query: String,
    },
    Doctor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum RendererChoice {
    Auto,
    Cells,
    Halfblock,
    Quadrant,
    Braille,
}

impl From<RendererChoice> for Backend {
    fn from(value: RendererChoice) -> Self {
        match value {
            RendererChoice::Auto => Self::Auto,
            RendererChoice::Cells => Self::Cells,
            RendererChoice::Halfblock => Self::Halfblock,
            RendererChoice::Quadrant => Self::Quadrant,
            RendererChoice::Braille => Self::Braille,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ColorChoice {
    Auto,
    Truecolor,
    #[value(name = "256")]
    Ansi256,
    #[value(name = "16")]
    Ansi16,
    Mono,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl fmt::Display for LogLevel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        })
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SnapshotFormat {
    Cells,
    Ansi,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DumpFormat {
    Dom,
    Cells,
    Accessibility,
}

#[derive(Debug, Clone, Copy)]
struct ViewportArgument {
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy)]
struct ExternalViewportPlan {
    viewport: ViewportArgument,
    device_scale: f64,
}

impl FromStr for ViewportArgument {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (width, height) = value
            .split_once(['x', 'X'])
            .ok_or_else(|| "viewport must use WIDTHxHEIGHT".to_owned())?;
        let width = width
            .parse::<u32>()
            .map_err(|_| "viewport width must be an integer".to_owned())?;
        let height = height
            .parse::<u32>()
            .map_err(|_| "viewport height must be an integer".to_owned())?;
        if width == 0 || height == 0 || width > 32_768 || height > 32_768 {
            return Err("viewport dimensions must be between 1 and 32768".to_owned());
        }
        Ok(Self { width, height })
    }
}

#[derive(Debug, Clone, Copy)]
struct ByteSize(u64);

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase();
        let (number, multiplier) = [
            ("gib", 1024_u64.pow(3)),
            ("gb", 1000_u64.pow(3)),
            ("mib", 1024_u64.pow(2)),
            ("mb", 1000_u64.pow(2)),
            ("kib", 1024_u64),
            ("kb", 1000_u64),
            ("b", 1_u64),
        ]
        .into_iter()
        .find_map(|(suffix, multiplier)| {
            normalized
                .strip_suffix(suffix)
                .map(|number| (number, multiplier))
        })
        .unwrap_or((normalized.as_str(), 1));
        let number = number.trim().parse::<u64>().map_err(|_| {
            "data limit must be an integer with an optional KiB/MiB/GiB suffix".to_owned()
        })?;
        number
            .checked_mul(multiplier)
            .map(Self)
            .ok_or_else(|| "data limit overflows u64".to_owned())
    }
}

#[derive(Debug, Error)]
enum AppError {
    #[error("configuration failed: {0}")]
    Config(#[from] ConfigError),
    #[error("platform initialization failed: {0}")]
    Platform(#[from] PlatformError),
    #[error("external Chrome operation failed: {0}")]
    External(#[from] external::ExternalError),
    #[error("external Chrome interactive session failed: {0}")]
    ExternalInteractive(#[from] external_interactive::ExternalInteractiveError),
    #[error("URL operation failed: {0}")]
    Url(#[from] UrlError),
    #[error("I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("JSON serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("logging initialization failed: {0}")]
    Logging(String),
    #[error("requested viewport scale must be finite and positive")]
    InvalidScale,
    #[error("data limit cannot be represented by this platform")]
    DataLimit,
}

impl AppError {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Config(_) | Self::InvalidScale | Self::DataLimit => 2,
            Self::Platform(_) => 40,
            Self::External(
                external::ExternalError::UnsupportedOutput { .. }
                | external::ExternalError::ProxyBypassWithoutProxy
                | external::ExternalError::TargetCredentialsDisallowed
                | external::ExternalError::InvalidDeviceScale
                | external::ExternalError::InvalidTerminalGeometry
                | external::ExternalError::InvalidRenderLimits
                | external::ExternalError::DomOutputLimit { .. },
            ) => 2,
            Self::External(_) => ErrorKind::Internal.automation_exit_code(),
            Self::ExternalInteractive(
                external_interactive::ExternalInteractiveError::InvalidLimits
                | external_interactive::ExternalInteractiveError::InvalidRequest { .. }
                | external_interactive::ExternalInteractiveError::TargetCredentialsDisallowed
                | external_interactive::ExternalInteractiveError::ProxyBypassWithoutProxy
                | external_interactive::ExternalInteractiveError::UnsupportedBackend { .. },
            ) => 2,
            Self::ExternalInteractive(
                external_interactive::ExternalInteractiveError::Platform(_),
            ) => 40,
            Self::ExternalInteractive(external_interactive::ExternalInteractiveError::Url(_)) => 10,
            Self::ExternalInteractive(_) => ErrorKind::Internal.automation_exit_code(),
            Self::Url(_) => 10,
            Self::Io(_) | Self::Json(_) | Self::Logging(_) => {
                ErrorKind::Internal.automation_exit_code()
            }
        }
    }
}

#[derive(Debug)]
struct RuntimeOptions {
    config: Config,
    config_path: PathBuf,
    capabilities: TerminalCapabilities,
    backend: Backend,
    columns: u16,
    rows: u16,
    device_scale: f64,
    limits: ResourceLimits,
}

const EXTERNAL_CHROME_EXECUTION: &str = "termglide <URL>";
const EXTERNAL_CHROME_DISCOVERY_RECOVERY: &str = "Install Google Chrome or Chromium, or rerun `termglide --browser-executable /absolute/path/to/chrome doctor`.";
const EXTERNAL_CHROME_SELECTED_PATH_RECOVERY: &str = "Verify the selected Chrome/Chromium binary can launch, then rerun `termglide --browser-executable <selected-path> doctor`.";
const EXTERNAL_CHROME_OVERRIDE_RECOVERY: &str = "Verify `--browser-executable` names an executable Chrome/Chromium binary, then rerun `termglide --browser-executable /absolute/path/to/chrome doctor`.";

#[derive(Debug, Serialize)]
struct ExternalChromeDoctor {
    status: &'static str,
    requested_executable: Option<String>,
    selected_executable: Option<String>,
    browser_version: Option<external::ExternalBrowserMetadata>,
    execution: &'static str,
    recovery: &'static str,
    error: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    install_panic_restore_hook();
    let cli = Cli::parse();
    if let Err(error) = initialize_logging(cli.log_level, cli.trace.as_deref()) {
        eprintln!("termglide: {error}");
        return ExitCode::from(error.exit_code());
    }
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("termglide: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

async fn run(cli: Cli) -> Result<(), AppError> {
    let runtime = runtime_options(&cli)?;
    if matches!(cli.command, Some(Command::Doctor)) {
        return print_doctor(&cli, &runtime).await;
    }
    run_chrome(&cli, &runtime).await
}

async fn run_chrome(cli: &Cli, runtime: &RuntimeOptions) -> Result<(), AppError> {
    if is_interactive_request(cli) {
        let target = command_target(cli, &runtime.config)?;
        let request = external_request(cli, runtime, &target);
        external_interactive::run_external_interactive_terminal(
            request,
            &Cancellation::new(),
            external_interactive::ExternalInteractiveLimits::default(),
        )
        .await?;
        return Ok(());
    }

    let target = command_target(cli, &runtime.config)?;
    let output = external_output_kind(cli);
    let page = external::render_for_output(external_request(cli, runtime, &target), output).await?;
    dispatch_external_output(cli, &page)
}

fn is_interactive_request(cli: &Cli) -> bool {
    should_enter_interactive(
        cli.command.is_some(),
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
    )
}

const fn should_enter_interactive(
    command_present: bool,
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
) -> bool {
    !command_present && stdin_is_terminal && stdout_is_terminal
}

fn runtime_options(cli: &Cli) -> Result<RuntimeOptions, AppError> {
    let config_path = cli.config.clone().unwrap_or(default_config_path()?);
    let mut config = if cli.config.is_some() || config_path.exists() {
        Config::load(&config_path)?
    } else {
        Config::default()
    };
    if let Some(proxy) = &cli.proxy {
        config.network.proxy = proxy.clone();
    }
    if !cli.no_proxy.is_empty() {
        config.network.no_proxy = cli.no_proxy.clone();
    }
    config.validate()?;

    let environment: HashMap<String, String> = std::env::vars().collect();
    let mut capabilities = TerminalCapabilities::from_environment(&environment);
    apply_color_choice(&mut capabilities, cli.color);
    let requested = if matches!(cli.renderer, RendererChoice::Auto) {
        parse_backend(&config.render.backend)?
    } else {
        cli.renderer.into()
    };
    let backend = select_renderer(requested, &capabilities, &environment);
    let (columns, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let scale = cli.scale.unwrap_or(1.0);
    if !scale.is_finite() || scale <= 0.0 {
        return Err(AppError::InvalidScale);
    }
    let mut limits = ResourceLimits::BROWSER_DEFAULT;
    let configured_limit = cli.data_limit.map(|value| value.0).or_else(|| {
        (config.network.data_limit_bytes > 0).then_some(config.network.data_limit_bytes)
    });
    if let Some(limit) = configured_limit {
        limits.max_bytes = usize::try_from(limit).map_err(|_| AppError::DataLimit)?;
    }
    Ok(RuntimeOptions {
        config,
        config_path,
        capabilities,
        backend,
        columns: columns.max(1),
        rows: rows.max(1),
        device_scale: f64::from(scale),
        limits,
    })
}

fn apply_color_choice(capabilities: &mut TerminalCapabilities, choice: ColorChoice) {
    capabilities.color = match choice {
        ColorChoice::Auto => return,
        ColorChoice::Truecolor => ColorLevel::TrueColor,
        ColorChoice::Ansi256 => ColorLevel::Ansi256,
        ColorChoice::Ansi16 => ColorLevel::Ansi16,
        ColorChoice::Mono => ColorLevel::Monochrome,
    };
}

fn select_renderer(
    requested: Backend,
    capabilities: &TerminalCapabilities,
    environment: &HashMap<String, String>,
) -> Backend {
    if should_prefer_apple_terminal_quadrant(requested, capabilities, environment) {
        return Backend::Quadrant;
    }
    if !matches!(requested, Backend::Auto) {
        return requested;
    }
    if capabilities.supports_backend(Backend::Halfblock) {
        Backend::Halfblock
    } else if capabilities.supports_backend(Backend::Cells) {
        Backend::Cells
    } else {
        Backend::Text
    }
}

fn should_prefer_apple_terminal_quadrant(
    requested: Backend,
    capabilities: &TerminalCapabilities,
    environment: &HashMap<String, String>,
) -> bool {
    matches!(requested, Backend::Auto)
        && capabilities.supports_backend(Backend::Quadrant)
        && environment
            .get("TERM_PROGRAM")
            .is_some_and(|program| program.eq_ignore_ascii_case("Apple_Terminal"))
}

fn parse_backend(value: &str) -> Result<Backend, AppError> {
    match value.to_ascii_lowercase().as_str() {
        "auto" => Ok(Backend::Auto),
        "cells" => Ok(Backend::Cells),
        "halfblock" => Ok(Backend::Halfblock),
        "quadrant" => Ok(Backend::Quadrant),
        "braille" => Ok(Backend::Braille),
        _ => Err(AppError::Config(ConfigError::Validation(
            "render.backend is invalid".to_owned(),
        ))),
    }
}

fn command_target(cli: &Cli, config: &Config) -> Result<BrowserUrl, AppError> {
    let input = match &cli.command {
        Some(Command::Inspect { url_or_query })
        | Some(Command::Snapshot { url_or_query, .. })
        | Some(Command::Dump { url_or_query, .. }) => url_or_query.as_str(),
        Some(Command::Doctor) => config.browser.homepage.as_str(),
        None => cli
            .url_or_query
            .as_deref()
            .unwrap_or(config.browser.homepage.as_str()),
    };
    match classify_address(input)? {
        AddressInput::Url(url) => Ok(*url),
        AddressInput::SearchQuery(query) => {
            Ok(SearchEngine::new(config.browser.default_search.clone())?.resolve(&query)?)
        }
    }
}

fn external_request(
    cli: &Cli,
    runtime: &RuntimeOptions,
    target: &BrowserUrl,
) -> external::ExternalRenderRequest {
    let viewport = cli.viewport.map_or_else(
        || {
            external_default_viewport(
                runtime.backend,
                runtime.columns,
                runtime.rows,
                runtime.device_scale,
            )
        },
        |viewport| ExternalViewportPlan {
            viewport,
            device_scale: runtime.device_scale,
        },
    );
    external::ExternalRenderRequest {
        target: target.serialized().to_owned(),
        policies: external::ExternalEnginePolicies {
            executable_override: cli.browser_executable.clone(),
            proxy: (!runtime.config.network.proxy.trim().is_empty())
                .then(|| runtime.config.network.proxy.clone()),
            proxy_bypass: runtime.config.network.no_proxy.clone(),
            user_agent: cli.user_agent.clone(),
            disable_images: cli.disable_images,
            disable_javascript: cli.disable_javascript || !runtime.config.javascript.enabled,
        },
        viewport: external::ExternalViewport {
            width: viewport.viewport.width,
            height: viewport.viewport.height,
            device_scale: viewport.device_scale,
        },
        terminal_columns: runtime.columns,
        terminal_rows: runtime.rows,
        backend: runtime.backend,
        capabilities: runtime.capabilities,
        limits: external::ExternalRenderLimits {
            max_bytes: runtime.limits.max_bytes,
            max_cells: runtime.limits.max_elements,
        },
    }
}

fn external_default_viewport(
    backend: Backend,
    columns: u16,
    rows: u16,
    requested_device_scale: f64,
) -> ExternalViewportPlan {
    let (horizontal_samples, vertical_samples, scale_numerator, scale_denominator) = match backend {
        Backend::Quadrant => (2, 4, 3, 2),
        Backend::Braille => (2, 4, 1, 1),
        Backend::Auto | Backend::Text | Backend::Cells | Backend::Halfblock => (1, 2, 3, 2),
    };
    ExternalViewportPlan {
        viewport: ViewportArgument {
            width: u32::from(columns)
                .saturating_mul(horizontal_samples)
                .saturating_mul(scale_numerator)
                .div_ceil(scale_denominator)
                .clamp(1, 32_768),
            height: u32::from(rows)
                .saturating_mul(vertical_samples)
                .saturating_mul(scale_numerator)
                .div_ceil(scale_denominator)
                .clamp(1, 32_768),
        },
        device_scale: requested_device_scale,
    }
}

fn external_output_kind(cli: &Cli) -> external::ExternalOutputKind {
    match &cli.command {
        Some(Command::Inspect { .. }) => external::ExternalOutputKind::Inspect,
        Some(Command::Snapshot {
            format: SnapshotFormat::Cells,
            ..
        }) => external::ExternalOutputKind::SnapshotCells,
        Some(Command::Snapshot {
            format: SnapshotFormat::Ansi,
            ..
        }) => external::ExternalOutputKind::SnapshotAnsi,
        Some(Command::Dump {
            format: DumpFormat::Cells,
            ..
        }) => external::ExternalOutputKind::DumpCells,
        Some(Command::Dump {
            format: DumpFormat::Dom,
            ..
        }) => external::ExternalOutputKind::DumpDom,
        Some(Command::Dump {
            format: DumpFormat::Accessibility,
            ..
        }) => external::ExternalOutputKind::DumpAccessibility,
        Some(Command::Doctor) | None => external::ExternalOutputKind::RenderOnce,
    }
}

fn dispatch_external_output(
    cli: &Cli,
    page: &external::ExternalRenderedPage,
) -> Result<(), AppError> {
    match &cli.command {
        Some(Command::Inspect { .. }) => {
            #[derive(Serialize)]
            struct Inspection<'a> {
                url: &'a str,
                engine: &'static str,
                browser: &'a external::ExternalBrowserMetadata,
            }
            write_json_stdout(&Inspection {
                url: &page.target,
                engine: "chrome",
                browser: &page.browser,
            })
        }
        Some(Command::Snapshot { format, output, .. }) => {
            let bytes = match format {
                SnapshotFormat::Cells => page.cell_text()?.into_bytes(),
                SnapshotFormat::Ansi => page.ansi()?.to_vec(),
            };
            write_atomic(output, &bytes)
        }
        Some(Command::Dump { format, .. }) => match format {
            DumpFormat::Cells => write_stdout(page.cell_text()?.as_bytes()),
            DumpFormat::Dom => write_stdout(page.outer_html()?.as_bytes()),
            DumpFormat::Accessibility => {
                let tree = page
                    .accessibility
                    .as_ref()
                    .ok_or(external::ExternalError::MissingAccessibilityTree)?;
                write_json_stdout(tree)
            }
        },
        Some(Command::Doctor) => Ok(()),
        None => write_stdout(page.ansi()?),
    }
}

fn write_stdout(bytes: &[u8]) -> Result<(), AppError> {
    let mut stdout = io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()?;
    Ok(())
}

fn write_json_stdout<T: Serialize>(value: &T) -> Result<(), AppError> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_stdout(&bytes)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), AppError> {
    let parent = path
        .parent()
        .filter(|value| !value.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("snapshot");
    let temporary = parent.join(format!(".{file_name}.termglide.tmp"));
    std::fs::write(&temporary, bytes)?;
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _cleanup_result = std::fs::remove_file(&temporary);
        return Err(AppError::Io(error));
    }
    Ok(())
}

async fn print_doctor(cli: &Cli, runtime: &RuntimeOptions) -> Result<(), AppError> {
    #[derive(Serialize)]
    struct Doctor<'a> {
        version: &'a str,
        os: &'a str,
        architecture: &'a str,
        config_file: String,
        renderer: String,
        external_chrome: ExternalChromeDoctor,
    }
    let report = Doctor {
        version: env!("CARGO_PKG_VERSION"),
        os: std::env::consts::OS,
        architecture: std::env::consts::ARCH,
        config_file: runtime.config_path.display().to_string(),
        renderer: format!("{:?}", runtime.backend).to_ascii_lowercase(),
        external_chrome: doctor_external_chrome(cli.browser_executable.clone()).await,
    };
    write_json_stdout(&report)
}

async fn doctor_external_chrome(executable_override: Option<PathBuf>) -> ExternalChromeDoctor {
    let requested_executable = executable_override
        .as_ref()
        .map(|path| path.display().to_string());
    let diagnostic = external::diagnose(executable_override).await;
    external_chrome_doctor_report(requested_executable, diagnostic)
}

fn external_chrome_doctor_report(
    requested_executable: Option<String>,
    diagnostic: Result<external::ExternalEngineDiagnostic, external::ExternalError>,
) -> ExternalChromeDoctor {
    match diagnostic {
        Ok(external::ExternalEngineDiagnostic::Ready {
            executable,
            browser,
        }) => ExternalChromeDoctor {
            status: "ready",
            requested_executable,
            selected_executable: Some(executable.display().to_string()),
            browser_version: Some(browser),
            execution: EXTERNAL_CHROME_EXECUTION,
            recovery: EXTERNAL_CHROME_SELECTED_PATH_RECOVERY,
            error: None,
        },
        Ok(external::ExternalEngineDiagnostic::Unhealthy { executable, error }) => {
            ExternalChromeDoctor {
                status: "unhealthy",
                requested_executable,
                selected_executable: Some(executable.display().to_string()),
                browser_version: None,
                execution: EXTERNAL_CHROME_EXECUTION,
                recovery: EXTERNAL_CHROME_SELECTED_PATH_RECOVERY,
                error: Some(error.to_string()),
            }
        }
        Err(external::ExternalError::Engine(
            tg_browser::ExternalEngineError::ExecutableNotFound,
        )) => ExternalChromeDoctor {
            status: "unavailable",
            requested_executable,
            selected_executable: None,
            browser_version: None,
            execution: EXTERNAL_CHROME_EXECUTION,
            recovery: EXTERNAL_CHROME_DISCOVERY_RECOVERY,
            error: Some("could not discover a Google Chrome or Chromium executable".to_owned()),
        },
        Err(error) => {
            let recovery = if requested_executable.is_some() {
                EXTERNAL_CHROME_OVERRIDE_RECOVERY
            } else {
                EXTERNAL_CHROME_DISCOVERY_RECOVERY
            };
            ExternalChromeDoctor {
                status: "unavailable",
                requested_executable,
                selected_executable: None,
                browser_version: None,
                execution: EXTERNAL_CHROME_EXECUTION,
                recovery,
                error: Some(error.to_string()),
            }
        }
    }
}

fn initialize_logging(level: LogLevel, trace: Option<&Path>) -> Result<(), AppError> {
    let filter = EnvFilter::new(level.to_string());
    if let Some(path) = trace {
        if let Some(parent) = path.parent().filter(|value| !value.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::File::create(path)?;
        tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_ansi(false)
                    .with_writer(Mutex::new(file)),
            )
            .try_init()
            .map_err(|error| AppError::Logging(error.to_string()))
    } else {
        tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
            .try_init()
            .map_err(|error| AppError::Logging(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::error::Error;
    use std::path::PathBuf;
    use std::str::FromStr;

    use clap::Parser;
    use termglide::external::{
        ExternalBrowserMetadata, ExternalEngineDiagnostic, ExternalOutputKind,
    };
    use termglide::external_interactive::ExternalInteractiveError;
    use tg_browser::ExternalEngineError;
    use tg_core::ErrorKind;
    use tg_network::CdpError;
    use tg_terminal::{Backend, TerminalCapabilities};

    use super::{
        AppError, ByteSize, Cli, Command, ViewportArgument, external_chrome_doctor_report,
        external_default_viewport, external_output_kind, should_enter_interactive,
        should_prefer_apple_terminal_quadrant,
    };

    #[test]
    fn cli_contract_parses_global_and_automation_commands() -> Result<(), Box<dyn Error>> {
        let cli = Cli::try_parse_from([
            "termglide",
            "--viewport",
            "800x600",
            "dump",
            "--format",
            "dom",
            "https://example.com",
        ])?;
        assert!(matches!(cli.command, Some(Command::Dump { .. })));
        let viewport = cli.viewport.ok_or("missing viewport")?;
        assert_eq!((viewport.width, viewport.height), (800, 600));
        Ok(())
    }

    #[test]
    fn external_default_viewport_matches_projection_resolution() {
        let cells = external_default_viewport(Backend::Cells, 256, 75, 1.0);
        assert_eq!((cells.viewport.width, cells.viewport.height), (384, 225));
        let braille = external_default_viewport(Backend::Braille, 256, 75, 1.0);
        assert_eq!(
            (braille.viewport.width, braille.viewport.height),
            (512, 300)
        );
        let quadrant = external_default_viewport(Backend::Quadrant, 256, 75, 1.0);
        assert_eq!(
            (quadrant.viewport.width, quadrant.viewport.height),
            (768, 450)
        );
    }

    #[test]
    fn apple_terminal_auto_prefers_filled_quadrants() {
        let environment = HashMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("TERM_PROGRAM".to_owned(), "Apple_Terminal".to_owned()),
        ]);
        let capabilities = TerminalCapabilities::from_environment(&environment);
        assert!(should_prefer_apple_terminal_quadrant(
            Backend::Auto,
            &capabilities,
            &environment,
        ));
        assert!(!should_prefer_apple_terminal_quadrant(
            Backend::Halfblock,
            &capabilities,
            &environment,
        ));
    }

    #[test]
    fn doctor_report_retains_path_version_and_recovery() -> Result<(), Box<dyn Error>> {
        let ready = external_chrome_doctor_report(
            None,
            Ok(ExternalEngineDiagnostic::Ready {
                executable: PathBuf::from("/opt/chrome"),
                browser: ExternalBrowserMetadata {
                    protocol_version: "1.3".to_owned(),
                    product: "Chrome/123.0".to_owned(),
                    revision: "revision".to_owned(),
                    user_agent: "agent".to_owned(),
                    javascript_version: "v8".to_owned(),
                },
            }),
        );
        assert_eq!(ready.status, "ready");
        assert_eq!(ready.selected_executable.as_deref(), Some("/opt/chrome"));
        assert!(ready.execution.contains("termglide"));
        assert!(ready.recovery.contains("--browser-executable"));

        let unavailable = external_chrome_doctor_report(
            Some("/missing/chrome".to_owned()),
            Err(termglide::external::ExternalError::Engine(
                ExternalEngineError::ExecutableNotFound,
            )),
        );
        assert_eq!(unavailable.status, "unavailable");
        assert_eq!(
            unavailable.requested_executable.as_deref(),
            Some("/missing/chrome")
        );
        Ok(())
    }

    #[test]
    fn byte_and_viewport_parsers_reject_overflow_and_zero() {
        assert_eq!(
            ByteSize::from_str("2MiB").map(|value| value.0),
            Ok(2 * 1024 * 1024)
        );
        assert!(ByteSize::from_str("18446744073709551615GiB").is_err());
        assert!(ViewportArgument::from_str("0x10").is_err());
    }

    #[test]
    fn interactive_mode_requires_both_terminal_streams_and_no_command() {
        assert!(should_enter_interactive(false, true, true));
        assert!(!should_enter_interactive(false, false, true));
        assert!(!should_enter_interactive(false, true, false));
        assert!(!should_enter_interactive(true, true, true));
    }

    #[test]
    fn output_routing_identifies_dump_formats() -> Result<(), Box<dyn Error>> {
        let dom = Cli::try_parse_from([
            "termglide",
            "dump",
            "--format",
            "dom",
            "https://example.com",
        ])?;
        assert_eq!(external_output_kind(&dom), ExternalOutputKind::DumpDom);
        let accessibility = Cli::try_parse_from([
            "termglide",
            "dump",
            "--format",
            "accessibility",
            "https://example.com",
        ])?;
        assert_eq!(
            external_output_kind(&accessibility),
            ExternalOutputKind::DumpAccessibility
        );
        Ok(())
    }

    #[test]
    fn external_interactive_errors_use_typed_exit_categories() {
        let invalid_request =
            AppError::ExternalInteractive(ExternalInteractiveError::InvalidRequest {
                field: "terminal geometry",
            });
        assert_eq!(invalid_request.exit_code(), 2);
        let cdp_timeout =
            AppError::ExternalInteractive(ExternalInteractiveError::Cdp(CdpError::Timeout));
        assert_eq!(
            cdp_timeout.exit_code(),
            ErrorKind::Internal.automation_exit_code()
        );
    }
}
