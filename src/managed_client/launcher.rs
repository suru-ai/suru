//! Detached process launching and authenticated readiness.

use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, anyhow};

use crate::{
    build_identity,
    protocol::{LifecycleState, PROTOCOL_VERSION, ShutdownReason},
    runtime::protect_current_user_file,
};

use super::{
    ManagedClientConfig,
    lifecycle::{self, Registration},
};

const SERVER_LOG_FILE: &str = "server.log";
const SERVER_LOG_TAIL_BYTES: u64 = 8 * 1024;

pub(super) async fn ensure_server(
    config: &ManagedClientConfig,
    deadline: tokio::time::Instant,
) -> Result<Registration> {
    let launching_build_identity = build_identity::for_executable(&config.server_executable)
        .map_err(|error| {
            startup_error(
                config,
                &format!("could not identify the Suru server executable: {error:#}"),
            )
        })?;
    let mut spawned = None;
    loop {
        let probe_result = match tokio::time::timeout_at(deadline, lifecycle::probe(config)).await {
            Ok(result) => result,
            Err(_) => {
                return Err(startup_error(
                    config,
                    &format!(
                        "detached Suru server did not become ready within {:?}",
                        config.startup_timeout
                    ),
                ));
            }
        };
        let error = match probe_result {
            Ok(registration) => match registration.health.lifecycle {
                LifecycleState::Ready
                    if registration.health.build_identity != launching_build_identity =>
                {
                    spawned = None;
                    lifecycle::shutdown_registered_instance(
                        config,
                        &registration,
                        ShutdownReason::Replacement,
                        deadline,
                    )
                    .await?;
                    anyhow!("registered Suru server is being replaced")
                }
                LifecycleState::Ready
                    if registration.health.protocol_version != PROTOCOL_VERSION =>
                {
                    return Err(startup_error(
                        config,
                        &format!(
                            "registered Suru server protocol version {} is incompatible with client protocol version {PROTOCOL_VERSION}",
                            registration.health.protocol_version
                        ),
                    ));
                }
                LifecycleState::Ready => return Ok(registration),
                LifecycleState::Starting => {
                    anyhow!("registered Suru server is still starting")
                }
                LifecycleState::Stopping => anyhow!("registered Suru server is stopping"),
                LifecycleState::Failed => {
                    return Err(startup_error(
                        config,
                        "registered Suru server reported failed startup",
                    ));
                }
            },
            Err(error) => {
                if spawned.is_none() {
                    spawned = Some(spawn_detached(config).map_err(|spawn_error| {
                        startup_error(
                            config,
                            &format!("could not launch the detached Suru server: {spawn_error:#}"),
                        )
                    })?);
                }
                error
            }
        };
        let spawned_exit = match spawned.as_mut() {
            Some(process) => process
                .try_wait()
                .context("inspect detached server process")?,
            None => None,
        };
        if let Some(status) = spawned_exit {
            if lifecycle::channel_is_owned(config).unwrap_or(false) {
                spawned = None;
            } else {
                return Err(startup_error(
                    config,
                    &format!(
                        "detached Suru server exited before becoming ready ({})",
                        describe_exit(status)
                    ),
                ));
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(startup_error(
                config,
                &format!(
                    "detached Suru server did not become ready within {:?}: {error:#}",
                    config.startup_timeout
                ),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn spawn_detached(config: &ManagedClientConfig) -> Result<Child> {
    let runtime_dir = config.create_private_runtime_dir()?;

    let log_path = runtime_dir.join(SERVER_LOG_FILE);
    let mut log_options = OpenOptions::new();
    log_options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_options.mode(0o600);
    }
    let stdout = log_options
        .open(&log_path)
        .with_context(|| format!("open server log {log_path:?}"))?;
    protect_current_user_file(&log_path)?;
    let stderr = stdout.try_clone().context("clone server log handle")?;

    tracing::info!(
        executable = %config.server_executable.display(),
        "launching detached Suru server"
    );
    let mut command = Command::new(&config.server_executable);
    command
        .arg("__server")
        .arg("--state-dir")
        .arg(config.runtime.state_base_dir())
        .arg("--data-dir")
        .arg(config.runtime.data_base_dir())
        .arg("--channel")
        .arg(config.channel());
    if let Some(config_dir) = config.runtime.config_dir() {
        command.arg("--config-dir").arg(config_dir);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    configure_detached_process(&mut command);
    #[cfg(windows)]
    let _standard_handle_guard = StandardHandleInheritanceGuard::disable()?;
    command.spawn().context("spawn detached Suru server")
}

fn describe_exit(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by a signal".to_owned(),
        |code| format!("exit code {code}"),
    )
}

pub(super) fn startup_error(config: &ManagedClientConfig, message: &str) -> anyhow::Error {
    tracing::error!("{message}");
    let log_path = log_path(config);
    match read_log_tail(&log_path) {
        Ok(tail) if !tail.is_empty() => anyhow!(
            "{message}. Inspect {log_path:?} and retry.\nRecent server log ({log_path:?}):\n{tail}"
        ),
        _ => anyhow!("{message}. Inspect {log_path:?} and retry."),
    }
}

fn log_path(config: &ManagedClientConfig) -> PathBuf {
    config.state_dir().join(SERVER_LOG_FILE)
}

fn read_log_tail(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open server log {path:?}"))?;
    let length = file.metadata().context("read server log metadata")?.len();
    file.seek(SeekFrom::Start(
        length.saturating_sub(SERVER_LOG_TAIL_BYTES),
    ))
    .context("seek to recent server log")?;
    let mut tail = Vec::with_capacity(SERVER_LOG_TAIL_BYTES as usize);
    file.read_to_end(&mut tail)
        .context("read recent server log")?;
    Ok(String::from_utf8_lossy(&tail).trim().to_owned())
}

#[cfg(unix)]
fn configure_detached_process(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_detached_process(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
}

#[cfg(windows)]
static STANDARD_HANDLE_INHERITANCE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(windows)]
struct StandardHandleInheritanceGuard {
    _lock: std::sync::MutexGuard<'static, ()>,
    changed: Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>,
}

#[cfg(windows)]
impl StandardHandleInheritanceGuard {
    fn disable() -> Result<Self> {
        use windows_sys::Win32::{
            Foundation::{
                GetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE,
                SetHandleInformation,
            },
            System::Console::{
                GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
            },
        };

        let lock = STANDARD_HANDLE_INHERITANCE
            .lock()
            .map_err(|_| anyhow!("standard handle inheritance lock is poisoned"))?;
        let mut guard = Self {
            _lock: lock,
            changed: Vec::new(),
        };
        for standard_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            // SAFETY: GetStdHandle returns a borrowed process-wide handle.
            let handle = unsafe { GetStdHandle(standard_handle) };
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut flags = 0;
            // SAFETY: flags points to writable storage and handle is borrowed for this call.
            if unsafe { GetHandleInformation(handle, &mut flags) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("inspect standard handle inheritance");
            }
            if flags & HANDLE_FLAG_INHERIT == 0 {
                continue;
            }
            // SAFETY: this changes only the inheritance flag on a valid process handle.
            if unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("disable standard handle inheritance");
            }
            guard.changed.push((handle, flags));
        }
        Ok(guard)
    }
}

#[cfg(windows)]
impl Drop for StandardHandleInheritanceGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

        for (handle, flags) in self.changed.drain(..) {
            // SAFETY: each borrowed handle is process-wide and remains valid across spawn.
            unsafe {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, flags & HANDLE_FLAG_INHERIT);
            }
        }
    }
}
