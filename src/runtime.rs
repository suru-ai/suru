use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

const RUNTIME_FILE: &str = "runtime.json";
const LOCK_FILE: &str = "server.lock";

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
    channel: Channel,
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
            channel,
        })
    }

    pub fn with_data_dir(mut self, data_base_dir: impl AsRef<Path>) -> Self {
        self.data_base_dir = data_base_dir.as_ref().to_path_buf();
        self.data_dir = self.channel.resolve_root(&self.data_base_dir);
        self
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

    pub fn channel(&self) -> &str {
        self.channel.as_str()
    }

    pub fn descriptor_path(&self) -> PathBuf {
        self.state_dir.join(RUNTIME_FILE)
    }

    pub(crate) fn lock_path(&self) -> PathBuf {
        self.state_dir.join(LOCK_FILE)
    }

    pub(crate) fn create_private_runtime_dir(&self) -> Result<PathBuf> {
        let runtime_dir = &self.state_dir;
        fs::create_dir_all(runtime_dir)
            .with_context(|| format!("create runtime directory {runtime_dir:?}"))?;
        protect_current_user_directory(runtime_dir)?;
        fs::create_dir_all(&self.data_dir)
            .with_context(|| format!("create data directory {:?}", self.data_dir))?;
        protect_current_user_directory(&self.data_dir)?;
        Ok(runtime_dir.to_path_buf())
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
