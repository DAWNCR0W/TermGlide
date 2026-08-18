//! Isolated external Chrome/Chromium lifecycle support.
//!
//! This module deliberately launches an external engine with a new temporary profile and a
//! loopback-only DevTools endpoint. It does not select a destination, profile, credential store,
//! or arbitrary Chromium command-line argument on behalf of the caller. Every Chromium switch is
//! either a fixed supervised-launch default or derived from a typed, validated option.

use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, ErrorKind};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use tg_core::Cancellation;
use thiserror::Error;
use url::Url;

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PROFILE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);
const PROFILE_CLEANUP_POLL_INTERVAL: Duration = Duration::from_millis(25);
const PROFILE_CLEANUP_SETTLE_INTERVAL: Duration = Duration::from_millis(100);
const MAX_VIEWPORT_DIMENSION: u32 = 16_384;
const MAX_TEMPORARY_DIRECTORY_CREATION_ATTEMPTS: u8 = 32;
const PROFILE_PREFIX: &str = "termglide-cdp";
const SCRATCH_PREFIX: &str = "termglide-tmp";
const USER_DATA_DIR_ARGUMENT_PREFIX: &str = "--user-data-dir=";
const REMOTE_DEBUGGING_ADDRESS_ARGUMENT: &str = "--remote-debugging-address=127.0.0.1";
const REMOTE_DEBUGGING_PORT_ARGUMENT: &str = "--remote-debugging-port=0";
// Fixed privacy and lifecycle defaults. Dynamic caller values are appended only after typed
// validation and cannot replace these switches.
const SUPERVISED_BROWSER_ARGUMENTS: &[&str] = &[
    "--headless=new",
    REMOTE_DEBUGGING_ADDRESS_ARGUMENT,
    REMOTE_DEBUGGING_PORT_ARGUMENT,
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-sync",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-default-apps",
    "--disable-background-mode",
    "--password-store=basic",
];
#[cfg(target_os = "macos")]
const MACOS_MOCK_KEYCHAIN_ARGUMENT: &str = "--use-mock-keychain";
const UNIX_PATH_EXECUTABLE_NAMES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
    "chrome",
    "Google Chrome",
    "Chromium",
];
const WINDOWS_PATH_EXECUTABLE_NAMES: &[&str] = &[
    "chrome.exe",
    "chromium.exe",
    "google-chrome.exe",
    "google-chrome-stable.exe",
];
const MACOS_STANDARD_EXECUTABLE_PATHS: &[&str] = &[
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta",
    "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
];
const LINUX_STANDARD_EXECUTABLE_PATHS: &[&str] = &[
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
    "/snap/bin/chromium",
    "/opt/google/chrome/google-chrome",
    "/opt/google/chrome/chrome",
];
const WINDOWS_INSTALL_CANDIDATE_COMPONENTS: &[&[&str]] = &[
    &["Google", "Chrome", "Application", "chrome.exe"],
    &["Chromium", "Application", "chrome.exe"],
];

static TEMPORARY_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalEnginePlatform {
    Linux,
    Macos,
    Windows,
    Other,
}

#[derive(Debug, Default)]
struct ExternalEngineDiscoveryEnvironment {
    program_files: Option<PathBuf>,
    program_files_x86: Option<PathBuf>,
    local_app_data: Option<PathBuf>,
    path_directories: Vec<PathBuf>,
}

impl ExternalEngineDiscoveryEnvironment {
    fn from_values(
        platform: ExternalEnginePlatform,
        program_files: Option<OsString>,
        program_files_x86: Option<OsString>,
        local_app_data: Option<OsString>,
        path_directories: Vec<PathBuf>,
    ) -> Self {
        Self {
            program_files: environment_root_path(platform, program_files),
            program_files_x86: environment_root_path(platform, program_files_x86),
            local_app_data: environment_root_path(platform, local_app_data),
            path_directories: absolute_auto_discovery_directories(platform, path_directories),
        }
    }
}

fn environment_root_path(
    platform: ExternalEnginePlatform,
    value: Option<OsString>,
) -> Option<PathBuf> {
    value
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .filter(|path| is_absolute_path_for_platform(platform, path))
}

fn absolute_auto_discovery_directories(
    platform: ExternalEnginePlatform,
    directories: Vec<PathBuf>,
) -> Vec<PathBuf> {
    directories
        .into_iter()
        .filter(|directory| is_absolute_path_for_platform(platform, directory))
        .collect()
}

fn is_absolute_path_for_platform(platform: ExternalEnginePlatform, path: &Path) -> bool {
    match platform {
        ExternalEnginePlatform::Windows => is_windows_absolute_path(path),
        ExternalEnginePlatform::Linux | ExternalEnginePlatform::Macos => {
            is_posix_absolute_path(path)
        }
        ExternalEnginePlatform::Other => path.is_absolute(),
    }
}

fn is_posix_absolute_path(path: &Path) -> bool {
    path.as_os_str().to_string_lossy().starts_with("/")
}

fn is_windows_absolute_path(path: &Path) -> bool {
    let value = path.as_os_str().to_string_lossy();
    let bytes = value.as_bytes();
    value.starts_with(r"\\")
        || value.starts_with("//")
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
}

/// A loopback-only Chrome DevTools websocket endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevToolsEndpoint {
    /// The fixed loopback address supplied to the external engine.
    pub host: Ipv4Addr,
    /// The ephemeral remote-debugging port chosen by Chromium.
    pub port: u16,
    /// The browser websocket path published in `DevToolsActivePort`.
    pub websocket_path: String,
}

impl DevToolsEndpoint {
    /// Formats the endpoint for a CDP websocket client.
    pub fn websocket_url(&self) -> String {
        format!("ws://{}:{}{}", self.host, self.port, self.websocket_path)
    }
}

/// JavaScript policy for the isolated external engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExternalEngineJavaScriptPolicy {
    /// Keep Chromium's normal JavaScript behavior.
    #[default]
    Enabled,
    /// Ask Chromium not to execute JavaScript for this profile.
    Disabled,
}

/// Image loading policy for the isolated external engine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ExternalEngineImagePolicy {
    /// Keep Chromium's normal image loading behavior.
    #[default]
    Enabled,
    /// Ask Chromium not to load images for this profile.
    Disabled,
}

/// A bounded initial browser viewport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalEngineViewport {
    width: u32,
    height: u32,
}

impl ExternalEngineViewport {
    /// Creates a viewport accepted by the external-engine launcher.
    pub fn new(width: u32, height: u32) -> Result<Self, ExternalEngineError> {
        if width == 0
            || height == 0
            || width > MAX_VIEWPORT_DIMENSION
            || height > MAX_VIEWPORT_DIMENSION
        {
            return Err(ExternalEngineError::InvalidViewport { width, height });
        }

        Ok(Self { width, height })
    }

    /// Returns the viewport width in CSS pixels.
    pub const fn width(self) -> u32 {
        self.width
    }

    /// Returns the viewport height in CSS pixels.
    pub const fn height(self) -> u32 {
        self.height
    }
}

/// A proxy endpoint that cannot embed credentials or carry path/query/fragment data.
#[derive(Debug, Clone)]
pub struct ExternalEngineProxy {
    server: Url,
    bypass: Option<ExternalEngineProxyBypass>,
}

impl ExternalEngineProxy {
    /// Validates a proxy endpoint before it can become a Chromium command-line argument.
    pub fn new(server: Url) -> Result<Self, ExternalEngineError> {
        if !matches!(server.scheme(), "http" | "https" | "socks4" | "socks5") {
            return Err(ExternalEngineError::UnsupportedProxyScheme {
                scheme: server.scheme().to_owned(),
            });
        }
        if server.host_str().is_none() {
            return Err(ExternalEngineError::ProxyHostMissing);
        }
        if !server.username().is_empty() || server.password().is_some() {
            return Err(ExternalEngineError::ProxyCredentialsDisallowed);
        }
        if !matches!(server.path(), "" | "/") {
            return Err(ExternalEngineError::ProxyPathDisallowed);
        }
        if server.query().is_some() || server.fragment().is_some() {
            return Err(ExternalEngineError::ProxyQueryOrFragmentDisallowed);
        }

        Ok(Self {
            server,
            bypass: None,
        })
    }

    /// Returns the caller-selected proxy endpoint.
    pub const fn server(&self) -> &Url {
        &self.server
    }

    fn chrome_server_argument(&self) -> &str {
        self.server
            .as_str()
            .strip_suffix('/')
            .unwrap_or(self.server.as_str())
    }

    /// Attaches an explicit, validated proxy-bypass contract to this proxy endpoint.
    pub fn with_bypass(mut self, bypass: ExternalEngineProxyBypass) -> Self {
        self.bypass = Some(bypass);
        self
    }

    /// Returns the proxy-bypass contract, when one was selected.
    pub fn bypass(&self) -> Option<&ExternalEngineProxyBypass> {
        self.bypass.as_ref()
    }
}

/// An explicit, validated equivalent of a `NO_PROXY` value for one external-engine launch.
///
/// Chromium receives the entries as a semicolon-separated `--proxy-bypass-list` value. The
/// launcher never reads the ambient `NO_PROXY` environment variable, so a caller must choose the
/// entries that apply to this isolated process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalEngineProxyBypass {
    entries: Vec<String>,
}

impl ExternalEngineProxyBypass {
    /// Validates individual proxy-bypass entries.
    ///
    /// Entries may be host patterns such as `*.internal.example`, `<local>`, IP literals, or IP
    /// CIDR blocks. Delimiters, URL syntax, credentials, control characters, broad `*` bypasses,
    /// and duplicate entries are rejected.
    pub fn new<I, S>(entries: I) -> Result<Self, ExternalEngineError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut normalized_entries: Vec<String> = Vec::new();
        for entry in entries {
            let entry = validate_proxy_bypass_entry(entry.as_ref())?;
            if normalized_entries
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&entry))
            {
                return Err(ExternalEngineError::ProxyBypassDuplicateEntry);
            }
            normalized_entries.push(entry);
        }
        if normalized_entries.is_empty() {
            return Err(ExternalEngineError::ProxyBypassEmpty);
        }
        Ok(Self {
            entries: normalized_entries,
        })
    }

    /// Parses an explicit comma-separated `NO_PROXY`-style value.
    pub fn from_no_proxy(no_proxy: &str) -> Result<Self, ExternalEngineError> {
        Self::new(no_proxy.split(','))
    }

    /// Returns the validated entries in caller-selected order.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    fn chrome_bypass_list(&self) -> String {
        self.entries.join(";")
    }
}

/// Configuration for one isolated Chrome or Chromium process.
///
/// `initial_target` is required so the launch always creates a caller-selected page target. No
/// default web destination is supplied by this module.
#[derive(Debug, Clone)]
pub struct ExternalEngineLaunchOptions {
    /// Optional exact Chrome/Chromium executable or macOS `.app` bundle override.
    pub executable_override: Option<PathBuf>,
    /// The initial page passed as the final, positional Chromium argument.
    pub initial_target: Url,
    /// Optional initial viewport. Chromium's own default is used when this is absent.
    pub viewport: Option<ExternalEngineViewport>,
    /// Optional caller-selected user agent.
    pub user_agent: Option<String>,
    /// Optional proxy endpoint without embedded credentials.
    pub proxy: Option<ExternalEngineProxy>,
    /// JavaScript policy for the isolated process.
    pub javascript: ExternalEngineJavaScriptPolicy,
    /// Image loading policy for the isolated process.
    pub images: ExternalEngineImagePolicy,
    /// Maximum time to wait for `DevToolsActivePort` after spawning Chromium.
    pub startup_timeout: Duration,
    /// Maximum wait between cancellation/process-state/endpoint checks.
    pub poll_interval: Duration,
    /// Optional absolute parent directory for a unique temporary Chromium profile.
    ///
    /// The launched browser always receives the resulting fresh child as `--user-data-dir`; no
    /// caller-provided raw Chromium arguments are accepted.
    pub temporary_profile_parent: Option<PathBuf>,
}

impl ExternalEngineLaunchOptions {
    /// Creates a launch configuration with an enabled JavaScript/image policy.
    pub fn new(initial_target: Url) -> Self {
        Self {
            executable_override: None,
            initial_target,
            viewport: None,
            user_agent: None,
            proxy: None,
            javascript: ExternalEngineJavaScriptPolicy::Enabled,
            images: ExternalEngineImagePolicy::Enabled,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
            temporary_profile_parent: None,
        }
    }
}

/// A running external Chrome/Chromium process and its isolated temporary profile.
///
/// Keep this guard alive while the CDP adapter uses [`Self::endpoint`]. Calling [`Self::shutdown`]
/// (or dropping it) waits for process termination before removing the temporary profile.
#[derive(Debug)]
pub struct ExternalEngineProcess {
    child: Child,
    executable_path: PathBuf,
    endpoint: DevToolsEndpoint,
    profile_dir: PathBuf,
    scratch_dir: PathBuf,
    temporary_directories_cleaned: bool,
}

impl ExternalEngineProcess {
    /// Returns the exact discovered executable selected for this process.
    pub fn executable_path(&self) -> &Path {
        &self.executable_path
    }

    /// Returns the browser-level DevTools endpoint that was published by Chromium.
    pub const fn endpoint(&self) -> &DevToolsEndpoint {
        &self.endpoint
    }

    /// Returns the OS process identifier of the external browser.
    pub fn process_id(&self) -> u32 {
        self.child.id()
    }

    /// Returns the path of the profile dedicated to this process.
    ///
    /// The path no longer exists after a successful [`Self::shutdown`].
    pub fn isolated_profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    /// Checks whether Chromium crashed or exited.
    ///
    /// Once an exit is observed, temporary browser data is removed before returning the status.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>, ExternalEngineError> {
        let status = self
            .child
            .try_wait()
            .map_err(|source| ExternalEngineError::ProcessIo {
                operation: "poll",
                source,
            })?;
        if status.is_some() {
            self.cleanup_temporary_directories()?;
        }
        Ok(status)
    }

    /// Terminates Chromium, waits for it, then removes its temporary data.
    pub fn shutdown(&mut self) -> Result<(), ExternalEngineError> {
        terminate_child(&mut self.child)?;
        self.cleanup_temporary_directories()
    }

    fn cleanup_temporary_directories(&mut self) -> Result<(), ExternalEngineError> {
        if self.temporary_directories_cleaned {
            return Ok(());
        }
        remove_engine_directories(&self.profile_dir, &self.scratch_dir)?;
        self.temporary_directories_cleaned = true;
        Ok(())
    }
}

impl Drop for ExternalEngineProcess {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            tracing::warn!(%error, "failed to fully shut down the external browser engine on drop");
        }
    }
}

/// Errors returned while discovering, launching, or owning an external browser engine.
#[derive(Debug, Error)]
pub enum ExternalEngineError {
    #[error("browser executable override is not an executable regular file: {path}")]
    InvalidExecutableOverride { path: PathBuf },
    #[error(
        "could not discover a Google Chrome or Chromium executable; install one or pass --browser-executable <path-to-chrome-or-chromium>"
    )]
    ExecutableNotFound,
    #[error("failed to {operation} {path}: {source}")]
    Filesystem {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to start external browser executable {path}: {source}")]
    Spawn {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to {operation} external browser process: {source}")]
    ProcessIo {
        operation: &'static str,
        #[source]
        source: io::Error,
    },
    #[error("external browser launch was cancelled")]
    Cancelled,
    #[error("external browser startup timeout after {timeout:?} waiting for DevToolsActivePort")]
    StartupTimeout { timeout: Duration },
    #[error("external browser exited before DevTools became ready: {status:?}")]
    ProcessExited { status: ExitStatus },
    #[error("DevToolsActivePort is invalid: {reason}")]
    InvalidDevToolsActivePort { reason: &'static str },
    #[error("external browser startup timeout must be greater than zero")]
    ZeroStartupTimeout,
    #[error("external browser poll interval must be greater than zero")]
    ZeroPollInterval,
    #[error("viewport {width}x{height} is outside the supported range")]
    InvalidViewport { width: u32, height: u32 },
    #[error("external browser {field} contains a control character")]
    InvalidArgument { field: &'static str },
    #[error("initial target URLs with embedded credentials are not allowed")]
    InitialTargetCredentialsDisallowed,
    #[error("proxy scheme {scheme} is not supported")]
    UnsupportedProxyScheme { scheme: String },
    #[error("proxy endpoint must include a host")]
    ProxyHostMissing,
    #[error("proxy endpoints with embedded credentials are not allowed")]
    ProxyCredentialsDisallowed,
    #[error("proxy endpoint paths are not allowed")]
    ProxyPathDisallowed,
    #[error("proxy endpoint queries and fragments are not allowed")]
    ProxyQueryOrFragmentDisallowed,
    #[error("proxy bypass entries must not be empty")]
    ProxyBypassEmpty,
    #[error("proxy bypass entries must not be duplicated")]
    ProxyBypassDuplicateEntry,
    #[error("a global proxy bypass is not allowed")]
    ProxyBypassWildcardDisallowed,
    #[error("proxy bypass entries with credentials are not allowed")]
    ProxyBypassCredentialsDisallowed,
    #[error("proxy bypass entry is invalid: {reason}")]
    InvalidProxyBypassEntry { reason: &'static str },
    #[error("temporary profile parent must be an absolute path: {path}")]
    TemporaryProfileParentNotAbsolute { path: PathBuf },
    #[error("could not allocate a unique temporary browser profile")]
    ProfileAllocationExhausted,
    #[error("could not allocate a unique temporary browser scratch directory")]
    ScratchAllocationExhausted,
}

/// Finds Chrome/Chromium through an explicit override, platform-standard locations, then `PATH`.
pub fn discover_external_engine(
    executable_override: Option<&Path>,
) -> Result<PathBuf, ExternalEngineError> {
    let platform = current_external_engine_platform();
    let environment = current_discovery_environment(platform);
    let standard_paths = standard_executable_paths_for(platform, &environment);
    discover_executable_from(
        executable_override,
        &standard_paths,
        &environment.path_directories,
        platform,
    )
}

fn current_external_engine_platform() -> ExternalEnginePlatform {
    if cfg!(target_os = "linux") {
        ExternalEnginePlatform::Linux
    } else if cfg!(target_os = "macos") {
        ExternalEnginePlatform::Macos
    } else if cfg!(target_os = "windows") {
        ExternalEnginePlatform::Windows
    } else {
        ExternalEnginePlatform::Other
    }
}

fn current_discovery_environment(
    platform: ExternalEnginePlatform,
) -> ExternalEngineDiscoveryEnvironment {
    let path_directories = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    ExternalEngineDiscoveryEnvironment::from_values(
        platform,
        env::var_os("ProgramFiles"),
        env::var_os("ProgramFiles(x86)"),
        env::var_os("LOCALAPPDATA"),
        path_directories,
    )
}

/// Spawns Chrome/Chromium with a unique profile and waits for its loopback CDP endpoint.
pub fn launch_external_engine(
    options: &ExternalEngineLaunchOptions,
    cancellation: &Cancellation,
) -> Result<ExternalEngineProcess, ExternalEngineError> {
    if cancellation.is_cancelled() {
        return Err(ExternalEngineError::Cancelled);
    }
    validate_launch_options(options)?;

    let executable = discover_external_engine(options.executable_override.as_deref())?;
    let profile_dir = create_temporary_profile(options.temporary_profile_parent.as_deref())?;
    let scratch_dir = match create_temporary_scratch() {
        Ok(path) => path,
        Err(error) => {
            if let Err(cleanup_error) = remove_profile_dir(&profile_dir) {
                tracing::warn!(
                    error = %cleanup_error,
                    "failed to remove temporary profile after scratch allocation failure"
                );
            }
            return Err(error);
        }
    };
    if cancellation.is_cancelled() {
        remove_engine_directories(&profile_dir, &scratch_dir)?;
        return Err(ExternalEngineError::Cancelled);
    }

    let mut command = build_launch_command(&executable, options, &profile_dir, &scratch_dir);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(source) => {
            if let Err(cleanup_error) = remove_engine_directories(&profile_dir, &scratch_dir) {
                tracing::warn!(
                    error = %cleanup_error,
                    "failed to remove temporary browser data after spawn failure"
                );
            }
            return Err(ExternalEngineError::Spawn {
                path: executable.clone(),
                source,
            });
        }
    };

    let endpoint = match wait_for_devtools_endpoint(
        &mut child,
        &profile_dir,
        options.startup_timeout,
        options.poll_interval,
        cancellation,
    ) {
        Ok(endpoint) => endpoint,
        Err(error) => {
            cleanup_failed_launch(&mut child, &profile_dir, &scratch_dir);
            return Err(error);
        }
    };

    Ok(ExternalEngineProcess {
        child,
        executable_path: executable,
        endpoint,
        profile_dir,
        scratch_dir,
        temporary_directories_cleaned: false,
    })
}

fn validate_launch_options(
    options: &ExternalEngineLaunchOptions,
) -> Result<(), ExternalEngineError> {
    if options.startup_timeout.is_zero() {
        return Err(ExternalEngineError::ZeroStartupTimeout);
    }
    if options.poll_interval.is_zero() {
        return Err(ExternalEngineError::ZeroPollInterval);
    }
    if !options.initial_target.username().is_empty() || options.initial_target.password().is_some()
    {
        return Err(ExternalEngineError::InitialTargetCredentialsDisallowed);
    }
    validate_command_value("initial target", options.initial_target.as_str())?;
    if let Some(user_agent) = &options.user_agent {
        validate_command_value("user agent", user_agent)?;
    }
    if let Some(proxy) = &options.proxy {
        validate_command_value("proxy", proxy.server().as_str())?;
        if let Some(proxy_bypass) = proxy.bypass() {
            validate_command_value("proxy bypass list", &proxy_bypass.chrome_bypass_list())?;
        }
    }
    if let Some(profile_parent) = &options.temporary_profile_parent
        && !profile_parent.is_absolute()
    {
        return Err(ExternalEngineError::TemporaryProfileParentNotAbsolute {
            path: profile_parent.clone(),
        });
    }
    Ok(())
}

fn validate_command_value(field: &'static str, value: &str) -> Result<(), ExternalEngineError> {
    if value.chars().any(char::is_control) {
        return Err(ExternalEngineError::InvalidArgument { field });
    }
    Ok(())
}

fn validate_proxy_bypass_entry(entry: &str) -> Result<String, ExternalEngineError> {
    if entry.chars().any(char::is_control) {
        return Err(ExternalEngineError::InvalidProxyBypassEntry {
            reason: "contains a control character",
        });
    }
    let entry = entry.trim_matches(' ');
    if entry.is_empty() {
        return Err(ExternalEngineError::ProxyBypassEmpty);
    }
    if entry.chars().any(char::is_whitespace) {
        return Err(ExternalEngineError::InvalidProxyBypassEntry {
            reason: "contains whitespace",
        });
    }
    if entry.contains(',') || entry.contains(';') {
        return Err(ExternalEngineError::InvalidProxyBypassEntry {
            reason: "contains a list delimiter",
        });
    }
    if entry.contains('@') {
        return Err(ExternalEngineError::ProxyBypassCredentialsDisallowed);
    }
    if entry.contains("://") || entry.contains('?') || entry.contains('#') || entry.contains('\\') {
        return Err(ExternalEngineError::InvalidProxyBypassEntry {
            reason: "contains URL syntax",
        });
    }
    if entry == "*" {
        return Err(ExternalEngineError::ProxyBypassWildcardDisallowed);
    }
    if entry == "<local>" {
        return Ok(entry.to_owned());
    }
    if let Some((address, prefix)) = entry.split_once('/') {
        if entry.matches('/').count() != 1 {
            return Err(ExternalEngineError::InvalidProxyBypassEntry {
                reason: "contains more than one CIDR delimiter",
            });
        }
        let address = parse_proxy_bypass_ip(address)?;
        let prefix = prefix
            .parse::<u8>()
            .ok()
            .filter(|prefix| *prefix <= cidr_prefix_limit(address))
            .ok_or(ExternalEngineError::InvalidProxyBypassEntry {
                reason: "CIDR prefix is outside the address range",
            })?;
        return Ok(format!("{address}/{prefix}"));
    }
    if let Ok(address) = parse_proxy_bypass_ip(entry) {
        return Ok(address.to_string());
    }
    if entry.contains(':') {
        return Err(ExternalEngineError::InvalidProxyBypassEntry {
            reason: "contains an unsupported port or credential separator",
        });
    }
    if !is_valid_proxy_bypass_host_pattern(entry) {
        return Err(ExternalEngineError::InvalidProxyBypassEntry {
            reason: "is not a supported host pattern",
        });
    }

    Ok(entry.to_owned())
}

fn parse_proxy_bypass_ip(value: &str) -> Result<IpAddr, ExternalEngineError> {
    let value = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(value);
    value
        .parse::<IpAddr>()
        .map_err(|_| ExternalEngineError::InvalidProxyBypassEntry {
            reason: "IP address is invalid",
        })
}

fn cidr_prefix_limit(address: IpAddr) -> u8 {
    match address {
        IpAddr::V4(_) => 32,
        IpAddr::V6(_) => 128,
    }
}

fn is_valid_proxy_bypass_host_pattern(entry: &str) -> bool {
    let entry = entry
        .strip_prefix("*.")
        .or_else(|| entry.strip_prefix('.'))
        .unwrap_or(entry);
    !entry.is_empty()
        && !entry.starts_with('.')
        && !entry.ends_with('.')
        && entry.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

fn build_launch_command(
    executable: &Path,
    options: &ExternalEngineLaunchOptions,
    profile_dir: &Path,
    scratch_dir: &Path,
) -> Command {
    let mut command = Command::new(executable);
    command
        .args(external_engine_arguments(options, profile_dir))
        // A short, separately owned path keeps Chromium's Unix sockets below platform path limits
        // while retaining its branded scratch directories inside TermGlide's cleanup boundary.
        .env("TMPDIR", scratch_dir)
        .env("TMP", scratch_dir)
        .env("TEMP", scratch_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn external_engine_arguments(
    options: &ExternalEngineLaunchOptions,
    profile_dir: &Path,
) -> Vec<OsString> {
    let mut profile_argument = OsString::from(USER_DATA_DIR_ARGUMENT_PREFIX);
    profile_argument.push(profile_dir.as_os_str());

    let mut arguments = SUPERVISED_BROWSER_ARGUMENTS
        .iter()
        .map(OsString::from)
        .collect::<Vec<_>>();
    arguments.insert(3, profile_argument);
    #[cfg(target_os = "macos")]
    arguments.push(OsString::from(MACOS_MOCK_KEYCHAIN_ARGUMENT));

    if let Some(viewport) = options.viewport {
        arguments.push(OsString::from(format!(
            "--window-size={},{}",
            viewport.width, viewport.height
        )));
    }
    if let Some(user_agent) = &options.user_agent {
        arguments.push(OsString::from(format!("--user-agent={user_agent}")));
    }
    if let Some(proxy) = &options.proxy {
        arguments.push(OsString::from(format!(
            "--proxy-server={}",
            proxy.chrome_server_argument()
        )));
        if let Some(proxy_bypass) = proxy.bypass() {
            arguments.push(OsString::from(format!(
                "--proxy-bypass-list={}",
                proxy_bypass.chrome_bypass_list()
            )));
        }
    }
    if options.javascript == ExternalEngineJavaScriptPolicy::Disabled {
        arguments.push(OsString::from("--disable-javascript"));
    }
    if options.images == ExternalEngineImagePolicy::Disabled {
        arguments.push(OsString::from("--blink-settings=imagesEnabled=false"));
    }

    arguments.push(OsString::from(options.initial_target.as_str()));
    arguments
}

fn wait_for_devtools_endpoint(
    child: &mut Child,
    profile_dir: &Path,
    timeout: Duration,
    poll_interval: Duration,
    cancellation: &Cancellation,
) -> Result<DevToolsEndpoint, ExternalEngineError> {
    let active_port_path = profile_dir.join("DevToolsActivePort");
    let started = Instant::now();
    let mut malformed_active_port = None;

    loop {
        if cancellation.is_cancelled() {
            return Err(ExternalEngineError::Cancelled);
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|source| ExternalEngineError::ProcessIo {
                operation: "poll",
                source,
            })?
        {
            return Err(ExternalEngineError::ProcessExited { status });
        }

        match fs::read_to_string(&active_port_path) {
            Ok(contents) => match parse_devtools_active_port(&contents) {
                Ok(endpoint) => return Ok(endpoint),
                Err(error) => malformed_active_port = Some(error),
            },
            Err(source) if source.kind() == ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ExternalEngineError::Filesystem {
                    operation: "read DevToolsActivePort from",
                    path: active_port_path,
                    source,
                });
            }
        }

        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return match malformed_active_port {
                Some(error) => Err(error),
                None => Err(ExternalEngineError::StartupTimeout { timeout }),
            };
        }
        thread::sleep(poll_interval.min(timeout.saturating_sub(elapsed)));
    }
}

fn parse_devtools_active_port(contents: &str) -> Result<DevToolsEndpoint, ExternalEngineError> {
    let mut lines = contents.lines();
    let port_line = lines.next().map(str::trim).unwrap_or_default();
    let websocket_path = lines.next().map(str::trim).unwrap_or_default();
    if port_line.is_empty() {
        return Err(ExternalEngineError::InvalidDevToolsActivePort {
            reason: "port is missing",
        });
    }
    let port = port_line
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(ExternalEngineError::InvalidDevToolsActivePort {
            reason: "port is not a non-zero u16",
        })?;
    if !websocket_path.starts_with("/devtools/browser/") {
        return Err(ExternalEngineError::InvalidDevToolsActivePort {
            reason: "browser websocket path is missing",
        });
    }
    if websocket_path.chars().any(char::is_control) || websocket_path.contains(char::is_whitespace)
    {
        return Err(ExternalEngineError::InvalidDevToolsActivePort {
            reason: "browser websocket path contains whitespace or control characters",
        });
    }

    Ok(DevToolsEndpoint {
        host: Ipv4Addr::LOCALHOST,
        port,
        websocket_path: websocket_path.to_owned(),
    })
}

fn standard_executable_paths_for(
    platform: ExternalEnginePlatform,
    environment: &ExternalEngineDiscoveryEnvironment,
) -> Vec<PathBuf> {
    match platform {
        ExternalEnginePlatform::Macos => MACOS_STANDARD_EXECUTABLE_PATHS
            .iter()
            .map(PathBuf::from)
            .collect(),
        ExternalEnginePlatform::Linux => LINUX_STANDARD_EXECUTABLE_PATHS
            .iter()
            .map(PathBuf::from)
            .collect(),
        ExternalEnginePlatform::Windows => windows_standard_executable_paths(environment),
        ExternalEnginePlatform::Other => Vec::new(),
    }
}

fn windows_standard_executable_paths(
    environment: &ExternalEngineDiscoveryEnvironment,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for root in [
        environment.program_files.as_deref(),
        environment.program_files_x86.as_deref(),
        environment.local_app_data.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        for components in WINDOWS_INSTALL_CANDIDATE_COMPONENTS {
            paths.push(join_path_components(root, components));
        }
    }
    paths
}

fn join_path_components(root: &Path, components: &[&str]) -> PathBuf {
    components
        .iter()
        .fold(root.to_path_buf(), |path, component| path.join(component))
}

fn discover_executable_from(
    executable_override: Option<&Path>,
    standard_paths: &[PathBuf],
    path_directories: &[PathBuf],
    platform: ExternalEnginePlatform,
) -> Result<PathBuf, ExternalEngineError> {
    if let Some(executable_override) = executable_override {
        return resolve_executable_override(executable_override).ok_or_else(|| {
            ExternalEngineError::InvalidExecutableOverride {
                path: executable_override.to_path_buf(),
            }
        });
    }

    if let Some(path) = standard_paths
        .iter()
        .find(|path| is_executable_file(path))
        .cloned()
    {
        return Ok(path);
    }

    for directory in path_directories
        .iter()
        .filter(|directory| is_absolute_path_for_platform(platform, directory))
    {
        for name in path_executable_names_for(platform) {
            let candidate = directory.join(name);
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }

    Err(ExternalEngineError::ExecutableNotFound)
}

fn resolve_executable_override(executable_override: &Path) -> Option<PathBuf> {
    if is_executable_file(executable_override) {
        return Some(executable_override.to_path_buf());
    }
    if executable_override.extension() == Some(OsStr::new("app")) {
        let binary_name = executable_override.file_stem()?;
        let bundled_binary = executable_override
            .join("Contents")
            .join("MacOS")
            .join(binary_name);
        if is_executable_file(&bundled_binary) {
            return Some(bundled_binary);
        }
    }
    None
}

fn path_executable_names_for(platform: ExternalEnginePlatform) -> &'static [&'static str] {
    match platform {
        ExternalEnginePlatform::Windows => WINDOWS_PATH_EXECUTABLE_NAMES,
        ExternalEnginePlatform::Linux
        | ExternalEnginePlatform::Macos
        | ExternalEnginePlatform::Other => UNIX_PATH_EXECUTABLE_NAMES,
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn create_temporary_profile(parent: Option<&Path>) -> Result<PathBuf, ExternalEngineError> {
    let parent = parent.map(Path::to_path_buf).unwrap_or_else(env::temp_dir);
    create_temporary_directory(
        parent,
        PROFILE_PREFIX,
        "create temporary profile parent",
        "set owner-only permissions on temporary browser profile",
        "create temporary browser profile",
        ExternalEngineError::ProfileAllocationExhausted,
    )
}

fn create_temporary_scratch() -> Result<PathBuf, ExternalEngineError> {
    create_temporary_directory(
        temporary_scratch_parent(),
        SCRATCH_PREFIX,
        "create temporary browser scratch parent",
        "set owner-only permissions on temporary browser scratch directory",
        "create temporary browser scratch directory",
        ExternalEngineError::ScratchAllocationExhausted,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn temporary_scratch_parent() -> PathBuf {
    // Chrome builds Unix-domain socket paths below this directory. The platform temp environment
    // can be caller-scoped and deeply nested, so use the standard short root and own only a unique
    // child within it.
    PathBuf::from("/tmp")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn temporary_scratch_parent() -> PathBuf {
    env::temp_dir()
}

fn create_temporary_directory(
    parent: PathBuf,
    prefix: &str,
    create_parent_operation: &'static str,
    _permissions_operation: &'static str,
    create_operation: &'static str,
    allocation_error: ExternalEngineError,
) -> Result<PathBuf, ExternalEngineError> {
    fs::create_dir_all(&parent).map_err(|source| ExternalEngineError::Filesystem {
        operation: create_parent_operation,
        path: parent.clone(),
        source,
    })?;

    for _ in 0..MAX_TEMPORARY_DIRECTORY_CREATION_ATTEMPTS {
        let directory = parent.join(unique_temporary_directory_name(prefix));
        match fs::create_dir(&directory) {
            Ok(()) => {
                #[cfg(unix)]
                if let Err(source) = set_owner_only_permissions(&directory) {
                    if let Err(cleanup_error) = fs::remove_dir(&directory) {
                        tracing::warn!(
                            error = %cleanup_error,
                            "failed to remove temporary directory after restricting its permissions failed"
                        );
                    }
                    return Err(ExternalEngineError::Filesystem {
                        operation: _permissions_operation,
                        path: directory,
                        source,
                    });
                }
                return Ok(directory);
            }
            Err(source) if source.kind() == ErrorKind::AlreadyExists => {}
            Err(source) => {
                return Err(ExternalEngineError::Filesystem {
                    operation: create_operation,
                    path: directory,
                    source,
                });
            }
        }
    }

    Err(allocation_error)
}

#[cfg(unix)]
fn set_owner_only_permissions(directory: &Path) -> io::Result<()> {
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
}

fn unique_temporary_directory_name(prefix: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let counter = TEMPORARY_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{}-{timestamp}-{counter}", std::process::id())
}

fn terminate_child(child: &mut Child) -> Result<(), ExternalEngineError> {
    if child
        .try_wait()
        .map_err(|source| ExternalEngineError::ProcessIo {
            operation: "poll",
            source,
        })?
        .is_some()
    {
        return Ok(());
    }

    if let Err(kill_error) = child.kill() {
        if child
            .try_wait()
            .map_err(|source| ExternalEngineError::ProcessIo {
                operation: "poll after failed termination",
                source,
            })?
            .is_none()
        {
            return Err(ExternalEngineError::ProcessIo {
                operation: "terminate",
                source: kill_error,
            });
        }
        return Ok(());
    }

    child
        .wait()
        .map_err(|source| ExternalEngineError::ProcessIo {
            operation: "wait for termination",
            source,
        })?;
    Ok(())
}

fn remove_profile_dir(profile_dir: &Path) -> Result<(), ExternalEngineError> {
    remove_profile_dir_with(
        profile_dir,
        PROFILE_CLEANUP_TIMEOUT,
        PROFILE_CLEANUP_POLL_INTERVAL,
        PROFILE_CLEANUP_SETTLE_INTERVAL,
        |path| fs::remove_dir_all(path),
    )
}

fn remove_scratch_dir(scratch_dir: &Path) -> Result<(), ExternalEngineError> {
    remove_temporary_directory_with(
        scratch_dir,
        PROFILE_CLEANUP_TIMEOUT,
        PROFILE_CLEANUP_POLL_INTERVAL,
        PROFILE_CLEANUP_SETTLE_INTERVAL,
        TemporaryDirectoryCleanupContext {
            remove_operation: "remove temporary browser scratch directory",
            verify_operation: "verify temporary browser scratch directory removal",
            absence_error: "temporary browser scratch directory did not remain absent during cleanup",
        },
        |path| fs::remove_dir_all(path),
    )
}

fn remove_engine_directories(
    profile_dir: &Path,
    scratch_dir: &Path,
) -> Result<(), ExternalEngineError> {
    let scratch_cleanup = remove_scratch_dir(scratch_dir);
    let profile_cleanup = remove_profile_dir(profile_dir);
    match (profile_cleanup, scratch_cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(profile_error), Err(scratch_error)) => {
            tracing::warn!(
                error = %scratch_error,
                "temporary browser scratch cleanup also failed"
            );
            Err(profile_error)
        }
    }
}

fn remove_profile_dir_with<F>(
    profile_dir: &Path,
    timeout: Duration,
    poll_interval: Duration,
    settle_interval: Duration,
    remove: F,
) -> Result<(), ExternalEngineError>
where
    F: FnMut(&Path) -> io::Result<()>,
{
    remove_temporary_directory_with(
        profile_dir,
        timeout,
        poll_interval,
        settle_interval,
        TemporaryDirectoryCleanupContext {
            remove_operation: "remove temporary browser profile",
            verify_operation: "verify temporary browser profile removal",
            absence_error: "temporary browser profile did not remain absent during cleanup",
        },
        remove,
    )
}

struct TemporaryDirectoryCleanupContext {
    remove_operation: &'static str,
    verify_operation: &'static str,
    absence_error: &'static str,
}

fn remove_temporary_directory_with<F>(
    directory: &Path,
    timeout: Duration,
    poll_interval: Duration,
    settle_interval: Duration,
    context: TemporaryDirectoryCleanupContext,
    mut remove: F,
) -> Result<(), ExternalEngineError>
where
    F: FnMut(&Path) -> io::Result<()>,
{
    let started = Instant::now();
    // Chromium helpers can briefly recreate temporary data after the supervised parent exits. A
    // second sweep must observe it missing after the settle interval before cleanup is complete.
    let mut verify_absence = false;
    loop {
        match remove(directory) {
            Ok(()) => {
                if settle_interval.is_zero() {
                    return Ok(());
                }
                verify_absence = true;
            }
            Err(source) if source.kind() == ErrorKind::NotFound => {
                if verify_absence || settle_interval.is_zero() {
                    return Ok(());
                }
                verify_absence = true;
            }
            Err(source) => {
                verify_absence = false;
                let elapsed = started.elapsed();
                if elapsed >= timeout {
                    return Err(ExternalEngineError::Filesystem {
                        operation: context.remove_operation,
                        path: directory.to_path_buf(),
                        source,
                    });
                }
                thread::sleep(poll_interval.min(timeout.saturating_sub(elapsed)));
                continue;
            }
        }

        let elapsed = started.elapsed();
        if elapsed >= timeout {
            return Err(ExternalEngineError::Filesystem {
                operation: context.verify_operation,
                path: directory.to_path_buf(),
                source: io::Error::other(context.absence_error),
            });
        }
        thread::sleep(settle_interval.min(timeout.saturating_sub(elapsed)));
    }
}

fn cleanup_failed_launch(child: &mut Child, profile_dir: &Path, scratch_dir: &Path) {
    match terminate_child(child) {
        Ok(()) => {
            if let Err(error) = remove_engine_directories(profile_dir, scratch_dir) {
                tracing::warn!(error = %error, "failed to remove temporary browser data after startup failure");
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "failed to terminate browser after startup failure");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsString;
    use std::fs::{self, File};
    use std::net::Ipv4Addr;
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    use std::process::Command;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, ExternalEngineError> {
            create_temporary_profile(None).map(Self)
        }

        fn scratch() -> Result<Self, ExternalEngineError> {
            create_temporary_scratch().map(Self)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(unix)]
    fn mark_executable(path: &Path) -> std::io::Result<()> {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
    }

    #[cfg(not(unix))]
    fn mark_executable(path: &Path) -> std::io::Result<()> {
        fs::metadata(path)?;
        Ok(())
    }

    #[test]
    fn parses_browser_devtools_active_port() -> Result<(), Box<dyn std::error::Error>> {
        let endpoint = parse_devtools_active_port("49213\n/devtools/browser/session-id\n")?;

        assert_eq!(endpoint.host, Ipv4Addr::LOCALHOST);
        assert_eq!(endpoint.port, 49_213);
        assert_eq!(endpoint.websocket_path, "/devtools/browser/session-id");
        assert_eq!(
            endpoint.websocket_url(),
            "ws://127.0.0.1:49213/devtools/browser/session-id"
        );
        Ok(())
    }

    #[test]
    fn rejects_incomplete_devtools_active_port() {
        assert!(matches!(
            parse_devtools_active_port("9222\n"),
            Err(ExternalEngineError::InvalidDevToolsActivePort { .. })
        ));
        assert!(matches!(
            parse_devtools_active_port("0\n/devtools/browser/session-id\n"),
            Err(ExternalEngineError::InvalidDevToolsActivePort { .. })
        ));
    }

    #[test]
    fn discovery_miss_error_includes_explicit_override_recovery() {
        let message = ExternalEngineError::ExecutableNotFound.to_string();

        assert!(message.contains("--browser-executable <path-to-chrome-or-chromium>"));
    }

    #[test]
    fn discovery_prefers_explicit_regular_file() -> Result<(), Box<dyn std::error::Error>> {
        let root = TestDirectory::new()?;
        let explicit = root.path().join("custom-chrome");
        let standard = root.path().join("standard-chromium");
        File::create(&explicit)?;
        File::create(&standard)?;
        mark_executable(&explicit)?;
        mark_executable(&standard)?;

        let discovered = discover_executable_from(
            Some(&explicit),
            std::slice::from_ref(&standard),
            &[],
            ExternalEnginePlatform::Macos,
        )?;
        assert_eq!(discovered, explicit);
        Ok(())
    }

    #[test]
    fn discovery_uses_standard_path_before_path_candidates()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TestDirectory::new()?;
        let standard = root.path().join("standard-chromium");
        let path_directory = root.path().join("bin");
        fs::create_dir(&path_directory)?;
        let path_candidate = path_directory.join("chromium");
        File::create(&standard)?;
        File::create(&path_candidate)?;
        mark_executable(&standard)?;
        mark_executable(&path_candidate)?;

        let discovered = discover_executable_from(
            None,
            std::slice::from_ref(&standard),
            &[path_directory],
            ExternalEnginePlatform::Linux,
        )?;
        assert_eq!(discovered, standard);
        Ok(())
    }

    #[test]
    fn windows_standard_candidates_follow_allowed_environment_root_order() {
        let path_directory = PathBuf::from(r"C:\Tools");
        let environment = ExternalEngineDiscoveryEnvironment::from_values(
            ExternalEnginePlatform::Windows,
            Some(OsString::from(r"C:\Program Files")),
            Some(OsString::from(r"C:\Program Files (x86)")),
            Some(OsString::from(r"C:\Users\TermGlide\AppData\Local")),
            vec![path_directory.clone()],
        );

        let candidates =
            standard_executable_paths_for(ExternalEnginePlatform::Windows, &environment);
        let expected = vec![
            PathBuf::from(r"C:\Program Files")
                .join("Google")
                .join("Chrome")
                .join("Application")
                .join("chrome.exe"),
            PathBuf::from(r"C:\Program Files")
                .join("Chromium")
                .join("Application")
                .join("chrome.exe"),
            PathBuf::from(r"C:\Program Files (x86)")
                .join("Google")
                .join("Chrome")
                .join("Application")
                .join("chrome.exe"),
            PathBuf::from(r"C:\Program Files (x86)")
                .join("Chromium")
                .join("Application")
                .join("chrome.exe"),
            PathBuf::from(r"C:\Users\TermGlide\AppData\Local")
                .join("Google")
                .join("Chrome")
                .join("Application")
                .join("chrome.exe"),
            PathBuf::from(r"C:\Users\TermGlide\AppData\Local")
                .join("Chromium")
                .join("Application")
                .join("chrome.exe"),
        ];

        assert_eq!(candidates, expected);
        assert_eq!(environment.path_directories, vec![path_directory]);
        assert_eq!(
            path_executable_names_for(ExternalEnginePlatform::Windows),
            &[
                "chrome.exe",
                "chromium.exe",
                "google-chrome.exe",
                "google-chrome-stable.exe",
            ][..]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_search_uses_exe_names_in_order() -> Result<(), Box<dyn std::error::Error>> {
        let root = TestDirectory::new()?;
        let path_directory = root.path().join("windows-bin");
        fs::create_dir(&path_directory)?;
        let chrome = path_directory.join("chrome.exe");
        let chromium = path_directory.join("chromium.exe");
        File::create(&chrome)?;
        File::create(&chromium)?;
        mark_executable(&chrome)?;
        mark_executable(&chromium)?;

        let discovered = discover_executable_from(
            None,
            &[],
            &[path_directory],
            ExternalEnginePlatform::Windows,
        )?;
        assert_eq!(discovered, chrome);
        Ok(())
    }

    #[test]
    fn discovery_environment_discards_empty_roots_and_relative_path_entries() {
        let unix_absolute = PathBuf::from("/opt/termglide-bin");
        let unix_environment = ExternalEngineDiscoveryEnvironment::from_values(
            ExternalEnginePlatform::Linux,
            Some(OsString::new()),
            Some(OsString::new()),
            Some(OsString::new()),
            vec![
                PathBuf::new(),
                PathBuf::from("relative-bin"),
                unix_absolute.clone(),
            ],
        );

        assert!(unix_environment.program_files.is_none());
        assert!(unix_environment.program_files_x86.is_none());
        assert!(unix_environment.local_app_data.is_none());
        assert_eq!(unix_environment.path_directories, vec![unix_absolute]);

        let windows_absolute = PathBuf::from(r"C:\Tools");
        let windows_environment = ExternalEngineDiscoveryEnvironment::from_values(
            ExternalEnginePlatform::Windows,
            Some(OsString::from("relative-program-files")),
            Some(OsString::new()),
            Some(OsString::new()),
            vec![
                PathBuf::new(),
                PathBuf::from("relative-bin"),
                PathBuf::from(r"C:drive-relative"),
                PathBuf::from(r"\drive-relative"),
                windows_absolute.clone(),
            ],
        );

        let windows_candidates =
            standard_executable_paths_for(ExternalEnginePlatform::Windows, &windows_environment);
        assert!(windows_candidates.is_empty());
        assert!(windows_environment.program_files.is_none());
        assert!(windows_environment.program_files_x86.is_none());
        assert!(windows_environment.local_app_data.is_none());
        assert_eq!(windows_environment.path_directories, vec![windows_absolute]);
    }

    #[test]
    fn linux_standard_candidates_have_canonical_order() {
        let candidates = standard_executable_paths_for(
            ExternalEnginePlatform::Linux,
            &ExternalEngineDiscoveryEnvironment::default(),
        );

        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/usr/bin/google-chrome"),
                PathBuf::from("/usr/bin/google-chrome-stable"),
                PathBuf::from("/usr/bin/chromium"),
                PathBuf::from("/usr/bin/chromium-browser"),
                PathBuf::from("/snap/bin/chromium"),
                PathBuf::from("/opt/google/chrome/google-chrome"),
                PathBuf::from("/opt/google/chrome/chrome"),
            ]
        );
        assert_eq!(
            path_executable_names_for(ExternalEnginePlatform::Linux),
            &[
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
                "chrome",
                "Google Chrome",
                "Chromium",
            ][..]
        );
    }

    #[test]
    fn macos_standard_candidates_retain_canonical_order() {
        let candidates = standard_executable_paths_for(
            ExternalEnginePlatform::Macos,
            &ExternalEngineDiscoveryEnvironment::default(),
        );

        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
                PathBuf::from(
                    "/Applications/Google Chrome Beta.app/Contents/MacOS/Google Chrome Beta"
                ),
                PathBuf::from(
                    "/Applications/Google Chrome Canary.app/Contents/MacOS/Google Chrome Canary"
                ),
                PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_discovery_rejects_non_executable_files_with_typed_errors()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = TestDirectory::new()?;
        let candidate = root.path().join("non-executable-chromium");
        File::create(&candidate)?;
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o600))?;

        assert!(matches!(
            discover_executable_from(
                None,
                std::slice::from_ref(&candidate),
                &[],
                ExternalEnginePlatform::Linux,
            ),
            Err(ExternalEngineError::ExecutableNotFound)
        ));
        assert!(matches!(
            discover_executable_from(Some(&candidate), &[], &[], ExternalEnginePlatform::Linux),
            Err(ExternalEngineError::InvalidExecutableOverride { .. })
        ));
        Ok(())
    }

    #[test]
    fn launch_arguments_are_loopback_isolated_and_targeted()
    -> Result<(), Box<dyn std::error::Error>> {
        let profile = TestDirectory::new()?;
        let initial_target = "data:text/html,%3Cmain%3Elaunch-contract%3C%2Fmain%3E";
        let mut options = ExternalEngineLaunchOptions::new(Url::parse(initial_target)?);
        options.viewport = Some(ExternalEngineViewport::new(1280, 720)?);
        options.user_agent = Some("TermGlide Test Agent".to_owned());
        let proxy_url = Url::parse("socks5://127.0.0.1:9050")?;
        let proxy_argument = format!(
            "--proxy-server={}",
            proxy_url.as_str().trim_end_matches('/')
        );
        let proxy_bypass = ExternalEngineProxyBypass::from_no_proxy(
            "localhost,.internal.example,*.svc.example,127.0.0.1,10.0.0.0/8,<local>",
        )?;
        let proxy_bypass_argument =
            format!("--proxy-bypass-list={}", proxy_bypass.chrome_bypass_list());
        options.proxy = Some(ExternalEngineProxy::new(proxy_url)?.with_bypass(proxy_bypass));
        options.javascript = ExternalEngineJavaScriptPolicy::Disabled;
        options.images = ExternalEngineImagePolicy::Disabled;

        let scratch = profile.path().join("scratch");
        let command =
            build_launch_command(Path::new("/browser"), &options, profile.path(), &scratch);
        for variable in ["TMPDIR", "TMP", "TEMP"] {
            let value = command.get_envs().find_map(|(name, value)| {
                (name == OsStr::new(variable)).then_some(value).flatten()
            });
            assert_eq!(
                value,
                Some(scratch.as_os_str()),
                "supervised browser temp variable escaped the isolated scratch directory: {variable}"
            );
        }

        let arguments = external_engine_arguments(&options, profile.path());
        let arguments = arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        for &required in SUPERVISED_BROWSER_ARGUMENTS {
            assert_eq!(
                arguments
                    .iter()
                    .filter(|argument| argument.as_str() == required)
                    .count(),
                1,
                "required supervised argument was missing or duplicated: {required}"
            );
        }
        #[cfg(target_os = "macos")]
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| argument.as_str() == MACOS_MOCK_KEYCHAIN_ARGUMENT)
                .count(),
            1
        );
        #[cfg(not(target_os = "macos"))]
        assert!(
            !arguments
                .iter()
                .any(|argument| argument.as_str() == "--use-mock-keychain")
        );

        assert!(profile.path().is_absolute());
        let expected_profile_argument = format!(
            "{USER_DATA_DIR_ARGUMENT_PREFIX}{}",
            profile.path().display()
        );
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| argument.starts_with(USER_DATA_DIR_ARGUMENT_PREFIX))
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![expected_profile_argument.as_str()]
        );
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| argument.starts_with("--remote-debugging-address="))
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![REMOTE_DEBUGGING_ADDRESS_ARGUMENT]
        );
        assert_eq!(
            arguments
                .iter()
                .filter(|argument| argument.starts_with("--remote-debugging-port="))
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![REMOTE_DEBUGGING_PORT_ARGUMENT]
        );

        let mut switch_names = BTreeSet::new();
        for argument in arguments
            .iter()
            .filter(|argument| argument.starts_with("--"))
        {
            let switch_name = argument
                .split_once('=')
                .map_or(argument.as_str(), |(name, _)| name);
            assert!(
                switch_names.insert(switch_name),
                "duplicate Chromium switch in supervised launch contract: {switch_name}"
            );
        }
        for forbidden in [
            "--no-sandbox",
            "--disable-web-security",
            "--ignore-certificate-errors",
            "--allow-insecure-localhost",
            "--remote-debugging-address=0.0.0.0",
            "--remote-allow-origins=*",
        ] {
            assert!(
                !arguments
                    .iter()
                    .any(|argument| argument.as_str() == forbidden),
                "unsafe Chromium switch escaped the supervised launch contract: {forbidden}"
            );
        }
        assert!(arguments.contains(&"--window-size=1280,720".to_owned()));
        assert!(arguments.contains(&"--user-agent=TermGlide Test Agent".to_owned()));
        assert!(arguments.contains(&proxy_argument));
        assert!(arguments.contains(&proxy_bypass_argument));
        assert!(arguments.contains(&"--disable-javascript".to_owned()));
        assert!(arguments.contains(&"--blink-settings=imagesEnabled=false".to_owned()));
        assert_eq!(arguments.last().map(String::as_str), Some(initial_target));
        Ok(())
    }

    #[test]
    fn proxy_rejects_embedded_credentials() -> Result<(), Box<dyn std::error::Error>> {
        let proxy = Url::parse("http://user:secret@127.0.0.1:8080")?;
        assert!(matches!(
            ExternalEngineProxy::new(proxy),
            Err(ExternalEngineError::ProxyCredentialsDisallowed)
        ));
        Ok(())
    }

    #[test]
    fn proxy_rejects_paths_queries_and_fragments() -> Result<(), Box<dyn std::error::Error>> {
        assert!(matches!(
            ExternalEngineProxy::new(Url::parse("http://127.0.0.1:8080/proxy")?),
            Err(ExternalEngineError::ProxyPathDisallowed)
        ));
        assert!(matches!(
            ExternalEngineProxy::new(Url::parse("http://127.0.0.1:8080/?route=direct")?),
            Err(ExternalEngineError::ProxyQueryOrFragmentDisallowed)
        ));
        assert!(matches!(
            ExternalEngineProxy::new(Url::parse("http://127.0.0.1:8080/#fragment")?),
            Err(ExternalEngineError::ProxyQueryOrFragmentDisallowed)
        ));
        Ok(())
    }

    #[test]
    fn proxy_bypass_accepts_explicit_no_proxy_entries() -> Result<(), Box<dyn std::error::Error>> {
        let bypass = ExternalEngineProxyBypass::from_no_proxy(
            " localhost, .internal.example,*.svc.example,127.0.0.1,10.0.0.0/8,::1,<local>",
        )?;

        let entries = bypass
            .entries()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            vec![
                "localhost",
                ".internal.example",
                "*.svc.example",
                "127.0.0.1",
                "10.0.0.0/8",
                "::1",
                "<local>",
            ]
        );
        assert_eq!(
            bypass.chrome_bypass_list(),
            "localhost;.internal.example;*.svc.example;127.0.0.1;10.0.0.0/8;::1;<local>"
        );
        Ok(())
    }

    #[test]
    fn proxy_bypass_rejects_hostile_or_credential_like_entries() {
        for entry in [
            "user:secret@internal.example",
            "user:secret",
            "https://internal.example",
            "internal.example;*.evil.example",
            "internal.\nexample",
            "\tlocalhost",
            "\u{2028}localhost",
        ] {
            assert!(
                ExternalEngineProxyBypass::new([entry]).is_err(),
                "unsafe bypass entry unexpectedly accepted: {entry}"
            );
        }
        assert!(matches!(
            ExternalEngineProxyBypass::new(["user:secret@internal.example"]),
            Err(ExternalEngineError::ProxyBypassCredentialsDisallowed)
        ));
        assert!(matches!(
            ExternalEngineProxyBypass::new(["*"]),
            Err(ExternalEngineError::ProxyBypassWildcardDisallowed)
        ));
        assert!(matches!(
            ExternalEngineProxyBypass::new(["localhost", "LOCALHOST"]),
            Err(ExternalEngineError::ProxyBypassDuplicateEntry)
        ));
    }

    #[test]
    fn launch_options_reject_initial_target_credentials() -> Result<(), Box<dyn std::error::Error>>
    {
        let options =
            ExternalEngineLaunchOptions::new(Url::parse("https://user:secret@example.invalid/")?);

        assert!(matches!(
            validate_launch_options(&options),
            Err(ExternalEngineError::InitialTargetCredentialsDisallowed)
        ));
        Ok(())
    }

    #[test]
    fn launch_options_reject_unsafe_caller_fields() -> Result<(), Box<dyn std::error::Error>> {
        let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
        options.user_agent = Some("TermGlide\n--no-sandbox".to_owned());
        assert!(matches!(
            validate_launch_options(&options),
            Err(ExternalEngineError::InvalidArgument {
                field: "user agent"
            })
        ));

        let mut options = ExternalEngineLaunchOptions::new(Url::parse("about:blank")?);
        options.temporary_profile_parent = Some(PathBuf::from("relative-profile-parent"));
        assert!(matches!(
            validate_launch_options(&options),
            Err(ExternalEngineError::TemporaryProfileParentNotAbsolute { .. })
        ));
        Ok(())
    }

    #[test]
    fn temporary_profiles_are_unique_and_removable() -> Result<(), Box<dyn std::error::Error>> {
        let root = TestDirectory::new()?;
        let first = create_temporary_profile(Some(root.path()))?;
        let second = create_temporary_profile(Some(root.path()))?;

        assert_ne!(first, second);
        assert!(first.is_dir());
        assert!(second.is_dir());
        remove_profile_dir(&first)?;
        remove_profile_dir(&second)?;
        assert!(!first.exists());
        assert!(!second.exists());
        Ok(())
    }

    #[test]
    fn temporary_scratch_uses_the_short_system_root() -> Result<(), Box<dyn std::error::Error>> {
        let scratch = TestDirectory::scratch()?;
        let expected_parent = temporary_scratch_parent();

        assert_eq!(scratch.path().parent(), Some(expected_parent.as_path()));
        assert!(
            scratch
                .path()
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with(SCRATCH_PREFIX))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn temporary_browser_directories_are_owner_only() -> Result<(), Box<dyn std::error::Error>> {
        let profile = TestDirectory::new()?;
        let scratch = TestDirectory::scratch()?;
        let permissions = fs::metadata(profile.path())?.permissions();
        let scratch_permissions = fs::metadata(scratch.path())?.permissions();

        assert_eq!(permissions.mode() & 0o777, 0o700);
        assert_eq!(scratch_permissions.mode() & 0o777, 0o700);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn supervised_shutdown_reaps_child_and_removes_temporary_directories()
    -> Result<(), Box<dyn std::error::Error>> {
        let profile = TestDirectory::new()?;
        let profile_dir = profile.path().to_path_buf();
        let scratch = TestDirectory::new()?;
        let scratch_dir = scratch.path().to_path_buf();
        let child = Command::new("/bin/sleep").arg("60").spawn()?;
        let mut process = ExternalEngineProcess {
            child,
            executable_path: PathBuf::from("/bin/sleep"),
            endpoint: DevToolsEndpoint {
                host: Ipv4Addr::LOCALHOST,
                port: 1,
                websocket_path: "/devtools/browser/test".to_owned(),
            },
            profile_dir: profile_dir.clone(),
            scratch_dir: scratch_dir.clone(),
            temporary_directories_cleaned: false,
        };

        assert_ne!(process.process_id(), 0);
        process.shutdown()?;
        assert!(process.try_wait()?.is_some());
        assert!(!profile_dir.exists());
        assert!(!scratch_dir.exists());
        Ok(())
    }

    #[test]
    fn profile_cleanup_retries_transient_filesystem_failures()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut attempts = 0usize;
        remove_profile_dir_with(
            Path::new("transient-profile-fixture"),
            Duration::from_secs(1),
            Duration::ZERO,
            Duration::ZERO,
            |_| {
                attempts += 1;
                if attempts < 3 {
                    Err(io::Error::other("transient profile writer"))
                } else {
                    Ok(())
                }
            },
        )?;

        assert_eq!(attempts, 3);
        Ok(())
    }

    #[test]
    fn profile_cleanup_removes_a_directory_recreated_after_the_first_sweep()
    -> Result<(), Box<dyn std::error::Error>> {
        let profile = TestDirectory::new()?;
        let profile_dir = profile.path().to_path_buf();
        let mut attempts = 0usize;

        remove_profile_dir_with(
            &profile_dir,
            Duration::from_secs(1),
            Duration::from_millis(1),
            Duration::from_millis(1),
            |path| {
                attempts += 1;
                fs::remove_dir_all(path)?;
                if attempts == 1 {
                    fs::create_dir(path)?;
                }
                Ok(())
            },
        )?;

        assert_eq!(attempts, 3);
        assert!(!profile_dir.exists());
        Ok(())
    }

    #[test]
    fn cancellation_short_circuits_before_browser_discovery()
    -> Result<(), Box<dyn std::error::Error>> {
        let cancellation = Cancellation::new();
        cancellation.cancel();
        let options = ExternalEngineLaunchOptions::new(Url::parse("https://example.invalid/")?);

        assert!(matches!(
            launch_external_engine(&options, &cancellation),
            Err(ExternalEngineError::Cancelled)
        ));
        Ok(())
    }
}
