use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const RUNTIME_FILE: &str = "runtime.json";
const LOCK_FILE: &str = "server.lock";
const LAST_STOP_FILE: &str = "last-stop.json";

#[derive(Clone, Debug)]
struct Channel(String);

impl Channel {
    fn new(channel: impl Into<String>) -> Result<Self> {
        let channel = channel.into();
        let is_safe = !channel.is_empty()
            && channel != "."
            && channel != ".."
            && channel
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
        if !is_safe {
            bail!("channel must contain only letters, numbers, '.', '-', or '_'");
        }
        Ok(Self(channel))
    }

    fn as_str(&self) -> &str {
        &self.0
    }

    fn resolve_root(&self, base_dir: &Path) -> PathBuf {
        if self.0 == "release" {
            base_dir.to_path_buf()
        } else {
            base_dir.join(&self.0)
        }
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    state_base_dir: PathBuf,
    data_base_dir: PathBuf,
    state_dir: PathBuf,
    data_dir: PathBuf,
    config_dir: Option<PathBuf>,
    channel: Channel,
    /// Whether this process makes the state and data directories where they
    /// are missing, or only uses them as it finds them. See
    /// [`RuntimeConfig::launched_into_existing_dirs`].
    makes_dirs: bool,
    /// The Manual stop the Channel had last seen when this Server was
    /// launched, where its launcher read one. See
    /// [`RuntimeConfig::launched_after`].
    launched_after: Option<LastStop>,
}

impl RuntimeConfig {
    pub fn new(state_base_dir: impl AsRef<Path>, channel: impl Into<String>) -> Result<Self> {
        let channel = Channel::new(channel)?;
        let state_base_dir = state_base_dir.as_ref().to_path_buf();
        let state_dir = channel.resolve_root(&state_base_dir);
        Ok(Self {
            state_base_dir: state_base_dir.clone(),
            data_base_dir: state_base_dir,
            data_dir: state_dir.clone(),
            state_dir,
            config_dir: None,
            channel,
            makes_dirs: true,
            launched_after: None,
        })
    }

    pub fn with_data_dir(mut self, data_base_dir: impl AsRef<Path>) -> Self {
        self.data_base_dir = data_base_dir.as_ref().to_path_buf();
        self.data_dir = self.channel.resolve_root(&self.data_base_dir);
        self
    }

    /// Points the server at the Config Document root. Unlike state and data,
    /// the config root is shared across Channels, so no Channel suffix is
    /// applied. Without this, no Config Documents load and every Setting
    /// keeps its built-in default.
    pub fn with_config_dir(mut self, config_dir: impl AsRef<Path>) -> Self {
        self.config_dir = Some(config_dir.as_ref().to_path_buf());
        self
    }

    /// Configures a Server that a launcher started, having made its state and
    /// data directories just before: such a Server makes neither, and ends
    /// rather than starting where either is missing.
    ///
    /// A directory missing by then was removed since the launch — the
    /// temporary directory of a test that has finished, say, or a user's
    /// state cleared away — and a Server that made it again would run on in
    /// a directory nobody expects it in, and that its launcher may already
    /// have finished with. Nothing else a Server writes makes either
    /// directory, launched or not: what it makes within them it makes only
    /// beneath a root that is still there (`paths::create_dir_beneath`), and
    /// a Server elected in a directory that is removed while it waits for the
    /// channel's lock finds that out once it holds the lock, and ends then
    /// (see `server::election`).
    pub fn launched_into_existing_dirs(mut self) -> Self {
        self.makes_dirs = false;
        self
    }

    /// Whether this process may make the state and data directories, rather
    /// than only using them as it finds them.
    pub(crate) fn makes_dirs(&self) -> bool {
        self.makes_dirs
    }

    /// Configures a Server whose launcher read `last_stop` as the Channel's
    /// last Manual stop just before launching it. A Manual stop is final for
    /// every Server launched before it, so one recorded since — however long
    /// this Server took to reach the election, and however long it then
    /// waited in it — ends this Server once it is elected, rather than
    /// letting it serve the Channel its user just stopped. Without this, a
    /// Server takes the last stop recorded as its launch is asked for, when
    /// `server::spawn` or any of its kin is called.
    pub fn launched_after(mut self, last_stop: LastStop) -> Self {
        self.launched_after = Some(last_stop);
        self
    }

    /// The Manual stop this Server's launcher saw before launching it, if it
    /// was launched having read one.
    pub(crate) fn launched_after_stop(&self) -> Option<LastStop> {
        self.launched_after
    }

    pub(crate) fn state_base_dir(&self) -> &Path {
        &self.state_base_dir
    }

    pub(crate) fn data_base_dir(&self) -> &Path {
        &self.data_base_dir
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn config_dir(&self) -> Option<&Path> {
        self.config_dir.as_deref()
    }

    pub fn channel(&self) -> &str {
        self.channel.as_str()
    }

    pub fn descriptor_path(&self) -> PathBuf {
        self.state_dir.join(RUNTIME_FILE)
    }

    pub(crate) fn lock_path(&self) -> PathBuf {
        self.state_dir.join(LOCK_FILE)
    }

    /// Where the Channel's last Manual stop is recorded, beside its election
    /// lock and runtime descriptor.
    pub(crate) fn last_stop_path(&self) -> PathBuf {
        self.state_dir.join(LAST_STOP_FILE)
    }

    /// The Channel's last Manual stop as it is recorded now: none where no
    /// stop has been, and an error where the record cannot be read whole and
    /// decoded. It is only ever replaced whole, so an error is never a record
    /// caught half written.
    pub fn last_stop(&self) -> io::Result<LastStop> {
        LastStop::read(&self.last_stop_path())
    }

    /// Makes the state and data directories readable by the current user
    /// alone, making them first where they are missing — as a launcher does
    /// before it launches a Server, and an in-process Server does as it
    /// starts. A Server [launched into existing
    /// directories](RuntimeConfig::launched_into_existing_dirs) makes
    /// neither, and fails here instead where either is missing.
    pub fn create_private_runtime_dir(&self) -> Result<PathBuf> {
        let runtime_dir = &self.state_dir;
        self.ensure_dir(runtime_dir, "runtime")?;
        protect_current_user_directory(runtime_dir)?;
        self.ensure_dir(&self.data_dir, "data")?;
        protect_current_user_directory(&self.data_dir)?;
        Ok(runtime_dir.to_path_buf())
    }

    fn ensure_dir(&self, dir: &Path, role: &str) -> Result<()> {
        if self.makes_dirs {
            return fs::create_dir_all(dir)
                .with_context(|| format!("create {role} directory {dir:?}"));
        }
        match fs::metadata(dir) {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => bail!("{role} directory {dir:?} is not a directory"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => bail!(
                "{role} directory {dir:?} is gone: a launched server uses the directories its \
                 launcher made, and never makes them again"
            ),
            Err(error) => Err(error).with_context(|| format!("inspect {role} directory {dir:?}")),
        }
    }
}

/// The Manual stop last recorded on a Channel — a `suru server stop`, or
/// the operating system's signal answered as one — naming the Server it
/// stopped, or none where the Channel has never been stopped so.
///
/// A Manual stop is final for every Server launched before it (ADR 0002).
/// Servers that lose an election wait out the winner's hold on the
/// Channel's lock, and one still waiting when the winner is stopped would
/// otherwise take the Channel over the moment it is let go, undoing the stop
/// it was never told of. So a stopping Server records its stop here before
/// it lets the lock go, and a launch carries the last stop it saw: elected,
/// a Server finding a later stop recorded than its launch saw ends instead
/// of serving. Only a Manual stop is recorded. A Server replaced by another
/// build hands the Channel to whichever waiting Server takes it, and one
/// that no longer stands for its Channel speaks for no Channel at all.
///
/// On a launch's command line it is written `none`, or as the instance id of
/// the stopped Server.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LastStop(Option<Uuid>);

/// The record of a Manual stop, as it is written to a Channel's state
/// directory.
#[derive(Deserialize, Serialize)]
pub(crate) struct StopRecord {
    pub(crate) instance_id: Uuid,
}

impl LastStop {
    /// The last stop recorded at `path`.
    fn read(path: &Path) -> io::Result<Self> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self(None)),
            Err(error) => return Err(error),
        };
        let record: StopRecord = serde_json::from_reader(io::BufReader::new(file))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        Ok(Self(Some(record.instance_id)))
    }

    /// The Server instance this stop stopped, or none where no stop has been
    /// recorded.
    pub fn stopped_instance(&self) -> Option<Uuid> {
        self.0
    }

    /// The stop recorded in `now` that a launch that saw `self` must yield
    /// to: the instance it stopped, where `now` names a stop other than the
    /// one seen. A record gone missing since names nothing — Suru never
    /// removes one, so it was cleared from outside along with whatever else
    /// was — and is no reason to end a launch.
    pub fn superseded_by(&self, now: LastStop) -> Option<Uuid> {
        now.0.filter(|_| now != *self)
    }
}

impl std::fmt::Display for LastStop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(instance_id) => write!(formatter, "{instance_id}"),
            None => formatter.write_str("none"),
        }
    }
}

impl std::str::FromStr for LastStop {
    type Err = uuid::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "none" {
            return Ok(Self(None));
        }
        value.parse().map(|instance_id| Self(Some(instance_id)))
    }
}

#[cfg(unix)]
pub(crate) fn protect_current_user_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("protect runtime directory {path:?}"))
}

#[cfg(unix)]
pub(crate) fn protect_current_user_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protect runtime file {path:?}"))
}

#[cfg(windows)]
pub(crate) fn protect_current_user_directory(path: &Path) -> Result<()> {
    protect_windows_owner(path, true)
}

#[cfg(windows)]
pub(crate) fn protect_current_user_file(path: &Path) -> Result<()> {
    protect_windows_owner(path, false)
}

#[cfg(windows)]
fn protect_windows_owner(path: &Path, directory: bool) -> Result<()> {
    use std::{os::windows::ffi::OsStrExt, ptr};

    use anyhow::anyhow;
    use windows_sys::Win32::{
        Foundation::{ERROR_SUCCESS, LocalFree},
        Security::{
            ACL,
            Authorization::{
                ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
                SE_FILE_OBJECT, SetNamedSecurityInfoW,
            },
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl,
            PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        },
    };

    struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

    impl Drop for LocalSecurityDescriptor {
        fn drop(&mut self) {
            // SAFETY: the descriptor was allocated by
            // ConvertStringSecurityDescriptorToSecurityDescriptorW.
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    let current_user_sid = current_windows_user_sid()?;
    let sddl = if directory {
        format!("D:P(A;OICI;FA;;;{current_user_sid})")
    } else {
        format!("D:P(A;;FA;;;{current_user_sid})")
    };
    let sddl = sddl.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut descriptor = ptr::null_mut();
    // SAFETY: sddl is NUL-terminated and descriptor points to writable storage.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("build current-user ACL for {path:?}"));
    }
    let descriptor = LocalSecurityDescriptor(descriptor);

    let mut dacl_present = 0;
    let mut dacl_defaulted = 0;
    let mut dacl: *mut ACL = ptr::null_mut();
    // SAFETY: descriptor remains alive and all out-pointers reference initialized storage.
    if unsafe {
        GetSecurityDescriptorDacl(
            descriptor.0,
            &mut dacl_present,
            &mut dacl,
            &mut dacl_defaulted,
        )
    } == 0
        || dacl_present == 0
        || dacl.is_null()
    {
        return Err(anyhow!("current-user security descriptor has no DACL"));
    }

    let path_utf16 = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: path is NUL-terminated, dacl is owned by the live descriptor, and null owner,
    // group, and SACL pointers are permitted when only DACL flags are requested.
    let status = unsafe {
        SetNamedSecurityInfoW(
            path_utf16.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(std::io::Error::from_raw_os_error(status as i32))
            .with_context(|| format!("protect runtime path {path:?}"));
    }
    Ok(())
}

#[cfg(windows)]
fn current_windows_user_sid() -> Result<String> {
    use std::{mem, ptr, slice};

    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, LocalFree},
        Security::{
            Authorization::ConvertSidToStringSidW, GetTokenInformation, TOKEN_QUERY, TOKEN_USER,
            TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: the handle was returned by OpenProcessToken and is owned by this guard.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    let mut token = ptr::null_mut();
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle and token points to writable storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("open current process token");
    }
    let token = OwnedHandle(token);

    let mut required_bytes = 0;
    // SAFETY: a null buffer with length zero is the documented size-query operation.
    unsafe {
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required_bytes);
    }
    if required_bytes == 0 {
        return Err(std::io::Error::last_os_error()).context("size current user token data");
    }

    let word_count = (required_bytes as usize).div_ceil(mem::size_of::<usize>());
    let mut token_data = vec![0usize; word_count];
    // SAFETY: token_data is aligned, writable, and at least required_bytes long.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            token_data.as_mut_ptr().cast(),
            required_bytes,
            &mut required_bytes,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("read current user token data");
    }
    // SAFETY: GetTokenInformation initialized the buffer with a TOKEN_USER value.
    let token_user = unsafe { &*token_data.as_ptr().cast::<TOKEN_USER>() };

    let mut sid_string = ptr::null_mut();
    // SAFETY: token_user contains a valid SID and sid_string points to writable storage.
    if unsafe { ConvertSidToStringSidW(token_user.User.Sid, &mut sid_string) } == 0 {
        return Err(std::io::Error::last_os_error()).context("format current user SID");
    }
    struct LocalSidString(*mut u16);
    impl Drop for LocalSidString {
        fn drop(&mut self) {
            // SAFETY: the string was allocated by ConvertSidToStringSidW.
            unsafe {
                LocalFree(self.0.cast());
            }
        }
    }
    let sid_string = LocalSidString(sid_string);
    // SAFETY: ConvertSidToStringSidW returns a valid NUL-terminated UTF-16 string.
    let length = unsafe {
        let mut length = 0;
        while *sid_string.0.add(length) != 0 {
            length += 1;
        }
        length
    };
    // SAFETY: length was measured within the NUL-terminated allocation.
    String::from_utf16(unsafe { slice::from_raw_parts(sid_string.0, length) })
        .context("decode current user SID")
}
