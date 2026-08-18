//! Operating-system adapters for configuration paths, terminal ownership, and shutdown.

use std::env;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Once;

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use directories::ProjectDirs;
use thiserror::Error;

const BUTTON_ONLY_MOUSE_CAPTURE: &[u8] = b"\x1b[?1003l\x1b[?1002l\x1b[?1000h\x1b[?1015h\x1b[?1006h";

#[derive(Debug, Error)]
pub enum PlatformError {
    #[error("interactive mode requires a terminal on stdout")]
    NotATerminal,
    #[error("TERMGLIDE_CONFIG_HOME must be an absolute path")]
    RelativeConfigOverride,
    #[error("platform configuration directory is unavailable")]
    ProjectDirectoriesUnavailable,
    #[error("platform I/O failed: {0}")]
    Io(#[from] io::Error),
}

pub fn default_config_path() -> Result<PathBuf, PlatformError> {
    if let Some(value) = env::var_os("TERMGLIDE_CONFIG_HOME") {
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(PlatformError::RelativeConfigOverride);
        }
        return Ok(path.join("config.toml"));
    }
    let project = ProjectDirs::from("org", "TermGlide", "TermGlide")
        .ok_or(PlatformError::ProjectDirectoriesUnavailable)?;
    Ok(project.config_dir().join("config.toml"))
}

#[derive(Debug)]
pub struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    pub fn enter() -> Result<Self, PlatformError> {
        Self::enter_with_pointer_motion(true)
    }

    /// Enters terminal mode without enabling drag and pointer-motion reports.
    ///
    /// Screenshot-backed rendering does not need motion events for every pixel crossed. Button,
    /// wheel, focus, paste, and keyboard events remain enabled.
    pub fn enter_buttons_only() -> Result<Self, PlatformError> {
        Self::enter_with_pointer_motion(false)
    }

    fn enter_with_pointer_motion(pointer_motion: bool) -> Result<Self, PlatformError> {
        if !io::stdout().is_terminal() {
            return Err(PlatformError::NotATerminal);
        }

        enable_raw_mode()?;
        let mut stdout = io::stdout().lock();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            Hide,
            EnableBracketedPaste,
            EnableFocusChange
        ) {
            drop(stdout);
            restore_terminal_best_effort();
            return Err(PlatformError::Io(error));
        }
        let capture = if pointer_motion {
            execute!(stdout, EnableMouseCapture)
        } else {
            stdout.write_all(BUTTON_ONLY_MOUSE_CAPTURE)
        };
        if let Err(error) = capture {
            drop(stdout);
            restore_terminal_best_effort();
            return Err(PlatformError::Io(error));
        }
        if let Err(error) = stdout.flush() {
            drop(stdout);
            restore_terminal_best_effort();
            return Err(PlatformError::Io(error));
        }
        Ok(Self { active: true })
    }

    pub fn restore(&mut self) -> Result<(), PlatformError> {
        if self.active {
            self.active = false;
            restore_terminal_best_effort();
        }
        Ok(())
    }

    pub const fn is_active(&self) -> bool {
        self.active
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            restore_terminal_best_effort();
        }
    }
}

pub fn restore_terminal_best_effort() {
    let mut stdout = io::stdout().lock();
    let _write_result = execute!(
        stdout,
        DisableMouseCapture,
        DisableFocusChange,
        DisableBracketedPaste,
        Show,
        LeaveAlternateScreen
    );
    let _flush_result = stdout.flush();
    let _raw_result = disable_raw_mode();
}

pub fn install_panic_restore_hook() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            restore_terminal_best_effort();
            previous(panic_info);
        }));
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    Interrupt,
    Terminate,
    Hangup,
}

pub async fn shutdown_signal() -> io::Result<ShutdownReason> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                Ok(ShutdownReason::Interrupt)
            }
            value = terminate.recv() => {
                match value {
                    Some(()) => Ok(ShutdownReason::Terminate),
                    None => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "termination signal stream closed",
                    )),
                }
            }
            value = hangup.recv() => {
                match value {
                    Some(()) => Ok(ShutdownReason::Hangup),
                    None => Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "hangup signal stream closed",
                    )),
                }
            }
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(ShutdownReason::Interrupt)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io::{self, Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    use super::{
        BUTTON_ONLY_MOUSE_CAPTURE, PlatformError, TerminalGuard, install_panic_restore_hook,
    };

    const PTY_MODE_ENV: &str = "TG_PLATFORM_PTY_MODE";
    const PTY_READY: &[u8] = b"TERMGLIDE_PTY_READY";
    const PTY_DONE: &[u8] = b"TERMGLIDE_PTY_DONE";
    const CURSOR_POSITION_REQUEST: &[u8] = b"\x1b[6n";
    const CURSOR_POSITION_RESPONSE: &[u8] = b"\x1b[1;1R";
    const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
    const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
    const PTY_CHILD_TIMEOUT: Duration = Duration::from_secs(5);
    const PTY_CHILD_EXIT_GRACE: Duration = Duration::from_secs(1);
    const PTY_READER_TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn button_only_capture_disables_motion_modes() {
        assert!(
            BUTTON_ONLY_MOUSE_CAPTURE
                .windows(8)
                .any(|part| part == b"\x1b[?1003l")
        );
        assert!(
            BUTTON_ONLY_MOUSE_CAPTURE
                .windows(8)
                .any(|part| part == b"\x1b[?1002l")
        );
        assert!(
            !BUTTON_ONLY_MOUSE_CAPTURE
                .windows(8)
                .any(|part| part == b"\x1b[?1003h")
        );
    }

    #[test]
    #[allow(
        clippy::panic,
        reason = "the child intentionally verifies panic-time terminal recovery"
    )]
    fn pty_child() -> Result<(), Box<dyn Error>> {
        let Ok(mode) = std::env::var(PTY_MODE_ENV) else {
            return Ok(());
        };
        match mode.as_str() {
            "normal" => {
                let mut guard = TerminalGuard::enter()?;
                announce(PTY_READY)?;
                guard.restore()?;
            }
            "unwind" => {
                install_panic_restore_hook();
                let caught = std::panic::catch_unwind(|| -> Result<(), PlatformError> {
                    let _guard = TerminalGuard::enter()?;
                    announce(PTY_READY)?;
                    std::panic::panic_any("intentional terminal restoration test");
                });
                assert!(caught.is_err());
            }
            value => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unsupported PTY child mode: {value}"),
                )
                .into());
            }
        }
        announce(PTY_DONE)?;
        Ok(())
    }

    #[test]
    fn restores_terminal_after_normal_exit() -> Result<(), Box<dyn Error>> {
        assert_terminal_restored(&run_in_pty("normal")?);
        Ok(())
    }

    #[test]
    fn restores_terminal_during_unwind() -> Result<(), Box<dyn Error>> {
        assert_terminal_restored(&run_in_pty("unwind")?);
        Ok(())
    }

    fn announce(marker: &[u8]) -> Result<(), PlatformError> {
        let mut stdout = io::stdout().lock();
        stdout.write_all(marker)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
        Ok(())
    }

    fn run_in_pty(mode: &str) -> Result<Vec<u8>, Box<dyn Error>> {
        let pty_system = native_pty_system();
        let pair = pty_system.openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 640,
            pixel_height: 384,
        })?;
        let reader = pair.master.try_clone_reader()?;
        let writer = Arc::new(Mutex::new(Some(pair.master.take_writer()?)));
        let mut command = CommandBuilder::new(std::env::current_exe()?);
        command.args(["--exact", "tests::pty_child", "--nocapture"]);
        command.env(PTY_MODE_ENV, mode);
        command.env("TERM", "xterm-256color");
        let mut child = pair.slave.spawn_command(command)?;
        drop(pair.slave);

        let ready = Arc::new(AtomicBool::new(false));
        let reader_ready = Arc::clone(&ready);
        let done = Arc::new(AtomicBool::new(false));
        let reader_done = Arc::clone(&done);
        let reader_writer = Arc::clone(&writer);
        let (reader_sender, reader_receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let _ = reader_sender.send(read_pty_to_end(
                reader,
                reader_writer,
                reader_ready,
                reader_done,
            ));
        });

        // ConPTY asks the terminal for its cursor position before the child starts. The reader
        // answers that query and closes stdin only after it sees the child's final restoration
        // marker; closing it earlier delivers Ctrl-C to the child on Windows.
        let deadline = Instant::now() + PTY_CHILD_TIMEOUT;
        let mut status = None;
        while !done.load(Ordering::Acquire) {
            if let Some(child_status) = child.try_wait()? {
                status = Some(child_status);
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PTY child did not restore the terminal before the timeout",
                )
                .into());
            }
            thread::sleep(Duration::from_millis(10));
        }
        if status.is_none() && done.load(Ordering::Acquire) {
            let grace_deadline = Instant::now() + PTY_CHILD_EXIT_GRACE;
            while Instant::now() < grace_deadline {
                if let Some(child_status) = child.try_wait()? {
                    status = Some(child_status);
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        drop(
            writer
                .lock()
                .map_err(|_| io::Error::other("PTY writer lock was poisoned"))?
                .take(),
        );
        let exit_deadline = Instant::now() + PTY_CHILD_TIMEOUT;
        let status = match status {
            Some(status) => status,
            None => loop {
                if let Some(child_status) = child.try_wait()? {
                    break child_status;
                }
                if Instant::now() >= exit_deadline {
                    let _ = child.kill();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "PTY child did not exit after its input closed",
                    )
                    .into());
                }
                thread::sleep(Duration::from_millis(10));
            },
        };
        drop(pair.master);
        let output = match reader_receiver.recv_timeout(PTY_READER_TIMEOUT) {
            Ok(output) => output?,
            Err(RecvTimeoutError::Timeout) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "PTY reader did not observe EOF after the child exited",
                )
                .into());
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("PTY reader thread failed").into());
            }
        };
        if !status.success() {
            return Err(io::Error::other(format!(
                "PTY child failed with exit code {} and signal {:?}: {}",
                status.exit_code(),
                status.signal(),
                String::from_utf8_lossy(&output)
            ))
            .into());
        }
        assert!(ready.load(Ordering::Acquire));
        Ok(output)
    }

    fn read_pty_to_end(
        mut reader: Box<dyn Read + Send>,
        writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
        ready: Arc<AtomicBool>,
        done: Arc<AtomicBool>,
    ) -> io::Result<Vec<u8>> {
        let mut output = Vec::new();
        let mut buffer = [0_u8; 1024];
        let mut cursor_responses = 0_usize;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    output.extend_from_slice(&buffer[..count]);
                    let cursor_requests = output
                        .windows(CURSOR_POSITION_REQUEST.len())
                        .filter(|window| *window == CURSOR_POSITION_REQUEST)
                        .count();
                    while cursor_responses < cursor_requests {
                        let mut writer = writer
                            .lock()
                            .map_err(|_| io::Error::other("PTY writer lock was poisoned"))?;
                        let writer = writer.as_mut().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "PTY writer closed before answering cursor position request",
                            )
                        })?;
                        writer.write_all(CURSOR_POSITION_RESPONSE)?;
                        writer.flush()?;
                        cursor_responses += 1;
                    }
                    if output
                        .windows(PTY_READY.len())
                        .any(|window| window == PTY_READY)
                    {
                        ready.store(true, Ordering::Release);
                    }
                    if output
                        .windows(PTY_DONE.len())
                        .any(|window| window == PTY_DONE)
                    {
                        done.store(true, Ordering::Release);
                    }
                }
                Err(error) if error.raw_os_error() == Some(5) => break,
                Err(error) => return Err(error),
            }
        }
        Ok(output)
    }

    fn assert_terminal_restored(output: &[u8]) {
        let entered = output
            .windows(ENTER_ALTERNATE_SCREEN.len())
            .position(|part| part == ENTER_ALTERNATE_SCREEN);
        let restored = output
            .windows(LEAVE_ALTERNATE_SCREEN.len())
            .rposition(|part| part == LEAVE_ALTERNATE_SCREEN);
        assert!(
            output
                .windows(PTY_READY.len())
                .any(|part| part == PTY_READY)
        );
        assert!(output.windows(PTY_DONE.len()).any(|part| part == PTY_DONE));
        assert!(entered.is_some());
        assert!(restored.is_some());
        assert!(entered < restored);
    }
}
