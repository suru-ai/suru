//! The program a Server spawns for Git commands. On Windows, `git` on `PATH`
//! is usually Git for Windows' launcher, `<root>\cmd\git.exe`, which only sets
//! up an environment and starts the real `git.exe`: two process creations, each
//! scanned by antivirus, for every Git command. Spawning the real one with the
//! launcher's environment spares the first.
#[cfg(any(windows, test))]
use std::path::Path;
use std::{ffi::OsString, path::PathBuf, time::Duration};

/// A program Git commands are spawned as, and what each needs set in its
/// environment beside the Server's own.
pub(super) struct Executable {
    pub(super) program: PathBuf,
    pub(super) environment: Vec<(OsString, OsString)>,
}

impl Executable {
    pub(super) fn named(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            environment: Vec::new(),
        }
    }
}

/// The Git that `git` on `PATH` names.
#[cfg(not(windows))]
pub(super) async fn on_path(_timeout: Duration) -> Executable {
    Executable::named("git")
}

/// The Git that `git` on `PATH` names, past Git for Windows' launcher where it
/// is one: that Git says where its commands live, and the real `git.exe` beside
/// them is started as the launcher would start it. Anything else, or any
/// failure to tell, leaves `git` on `PATH`.
#[cfg(windows)]
pub(super) async fn on_path(timeout: Duration) -> Executable {
    let mut command = tokio::process::Command::new("git");
    // Where this Git keeps its commands, not where an override points them.
    command
        .arg("--exec-path")
        .env_remove("GIT_EXEC_PATH")
        .stdin(std::process::Stdio::null());
    let exec_path =
        match tokio::time::timeout(timeout, crate::process_tree::output(&mut command)).await {
            Ok(Ok(output)) if output.status.success() => String::from_utf8(output.stdout).ok(),
            _ => None,
        };
    let executable = exec_path
        .and_then(|exec_path| {
            past_launcher(
                Path::new(exec_path.trim_end_matches(['\r', '\n'])),
                |name| std::env::var_os(name),
                system_directory().as_deref(),
            )
        })
        .unwrap_or_else(|| Executable::named("git"));
    tracing::debug!(program = %executable.program.display(), "Git resolved");
    executable
}

/// The directory `GetSystemDirectoryW` names, which the launcher never takes
/// for a home.
#[cfg(windows)]
fn system_directory() -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

    let mut buffer = [0u16; 260];
    // SAFETY: buffer is writable for the length passed.
    let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    (length > 0 && length < buffer.len()).then(|| OsString::from_wide(&buffer[..length]).into())
}

/// The prefixes Git for Windows installs under, each named for the MSYS2
/// environment its programs are built for: x64 from Git for Windows 2.56 and
/// x64 before it, ARM64, and x86. Each launcher sets `MSYSTEM` to the name of
/// the one it was built for.
#[cfg(any(windows, test))]
const MSYSTEMS: [&str; 4] = ["UCRT64", "MINGW64", "CLANGARM64", "MINGW32"];

/// How Git for Windows' launcher would start the Git whose commands are in
/// `exec_path`. `None` unless that is `<root>\<prefix>\libexec\git-core` of an
/// installation with the launcher at `<root>\cmd\git.exe`; MSYS2's own Git has
/// the same prefix but no launcher, nor its environment.
///
/// The environment is what the launcher's `setup_environment` and
/// `maybe_read_config` set (git-for-windows/MINGW-packages,
/// `mingw-w64-git/git-wrapper.c`). The real `git.exe` sets much of it up
/// itself, but only where `MSYSTEM` is unset and only since Git for Windows
/// 2.26, so none of it is left to that.
#[cfg(any(windows, test))]
pub(super) fn past_launcher(
    exec_path: &Path,
    variable: impl Fn(&str) -> Option<OsString>,
    system_directory: Option<&Path>,
) -> Option<Executable> {
    if !exec_path.is_absolute() || !exec_path.ends_with(Path::new("libexec").join("git-core")) {
        return None;
    }
    // Git for Windows prints its exec path with forward slashes; rebuilt from
    // its components, the paths below are spelled as the launcher spells them.
    let exec_path = exec_path.components().collect::<PathBuf>();
    let prefix = exec_path.parent()?.parent()?;
    let name = prefix.file_name()?;
    let msystem = MSYSTEMS
        .into_iter()
        .find(|msystem| name.eq_ignore_ascii_case(msystem))?;
    let root = prefix.parent()?;
    let bin = root.join(msystem.to_ascii_lowercase()).join("bin");
    let program = bin.join("git.exe");
    if !program.is_file() || !root.join("cmd").join("git.exe").is_file() {
        return None;
    }
    let config = match std::fs::read_to_string(root.join("etc").join("git-bash.config")) {
        Ok(config) => config,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(_) => return None,
    };

    let mut environment = vec![("MSYSTEM".into(), msystem.into())];
    if variable("PLINK_PROTOCOL").is_none() {
        environment.push(("PLINK_PROTOCOL".into(), "ssh".into()));
    }
    let home = match variable("HOME") {
        Some(home) => Some(home),
        None => {
            let home = found_home(&variable, system_directory);
            if let Some(home) = &home {
                environment.push(("HOME".into(), home.clone()));
            }
            home
        }
    };
    let mut path = OsString::new();
    let home_bin = home.map(|home| Path::new(&home).join("bin"));
    for entry in [Some(bin), Some(root.join("usr").join("bin")), home_bin]
        .into_iter()
        .flatten()
    {
        path.push(entry);
        path.push(";");
    }
    path.push(variable("PATH").unwrap_or_default());
    environment.push(("PATH".into(), path));
    // Each `MSYS=` line goes before the options already set.
    let mut msys = None;
    for options in config.lines().filter_map(|line| line.strip_prefix("MSYS=")) {
        let mut combined = OsString::from(options);
        if let Some(existing) = msys.take().or_else(|| variable("MSYS")) {
            combined.push(" ");
            combined.push(existing);
        }
        msys = Some(combined);
    }
    environment.extend(msys.map(|msys| ("MSYS".into(), msys)));
    Some(Executable {
        program,
        environment,
    })
}

/// `HOME` where the launcher finds it unset: `HOMEDRIVE` and `HOMEPATH`
/// together where they name a directory other than the system directory,
/// otherwise `USERPROFILE`.
#[cfg(any(windows, test))]
fn found_home(
    variable: &impl Fn(&str) -> Option<OsString>,
    system_directory: Option<&Path>,
) -> Option<OsString> {
    if let Some(path) = variable("HOMEPATH") {
        let mut home = variable("HOMEDRIVE").unwrap_or_default();
        home.push(path);
        let system =
            system_directory.is_some_and(|system| system.as_os_str().eq_ignore_ascii_case(&home));
        if !system && Path::new(&home).is_dir() {
            return Some(home);
        }
    }
    variable("USERPROFILE")
}

#[cfg(test)]
mod tests {
    use super::{Executable, past_launcher};
    use std::{
        collections::HashMap,
        ffi::OsString,
        path::{MAIN_SEPARATOR_STR, Path, PathBuf},
    };

    /// A Git for Windows installation, as far as its launcher looks into it.
    struct Installation {
        _temporary: tempfile::TempDir,
        root: PathBuf,
    }
    impl Installation {
        fn new(prefix: &str) -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().join("Git");
            for program in [root.join("cmd"), root.join(prefix).join("bin")] {
                std::fs::create_dir_all(&program).unwrap();
                let program = program.join("git.exe");
                std::fs::write(program, "").unwrap();
            }
            std::fs::create_dir_all(root.join(prefix).join("libexec").join("git-core")).unwrap();
            Self {
                _temporary: temporary,
                root,
            }
        }
        fn exec_path(&self, prefix: &str) -> PathBuf {
            self.root.join(prefix).join("libexec").join("git-core")
        }
        fn configure(&self, contents: &str) {
            std::fs::create_dir_all(self.root.join("etc")).unwrap();
            std::fs::write(self.root.join("etc").join("git-bash.config"), contents).unwrap();
        }
    }

    fn launch(
        exec_path: &Path,
        variables: &[(&str, &OsString)],
        system_directory: Option<&Path>,
    ) -> Option<Executable> {
        let variables = variables
            .iter()
            .map(|(name, value)| (name.to_string(), (*value).clone()))
            .collect::<HashMap<_, _>>();
        past_launcher(
            exec_path,
            |name| variables.get(name).cloned(),
            system_directory,
        )
    }
    fn variable<'a>(executable: &'a Executable, name: &str) -> Option<&'a OsString> {
        executable
            .environment
            .iter()
            .find_map(|(set, value)| (set == name).then_some(value))
    }
    /// `PATH` as the launcher lays it out: its entries, each ended by `;`,
    /// before what the Server's `PATH` already holds.
    fn search_path(entries: &[PathBuf], after: &str) -> OsString {
        let mut path = OsString::new();
        for entry in entries {
            path.push(entry);
            path.push(";");
        }
        path.push(after);
        path
    }

    #[test]
    fn the_real_git_is_started_as_each_launcher_would_start_it() {
        let home = OsString::from("home");
        let path = OsString::from("system;tools");
        for (prefix, msystem) in [
            ("ucrt64", "UCRT64"),
            ("mingw64", "MINGW64"),
            ("clangarm64", "CLANGARM64"),
            ("mingw32", "MINGW32"),
        ] {
            let installation = Installation::new(prefix);
            let executable = launch(
                &installation.exec_path(prefix),
                &[("HOME", &home), ("PATH", &path)],
                None,
            )
            .unwrap_or_else(|| panic!("{prefix} is a Git for Windows layout"));
            let bin = installation.root.join(prefix).join("bin");
            assert_eq!(executable.program, bin.join("git.exe"), "{prefix}");
            assert_eq!(
                executable.environment,
                vec![
                    ("MSYSTEM".into(), msystem.into()),
                    ("PLINK_PROTOCOL".into(), "ssh".into()),
                    (
                        "PATH".into(),
                        search_path(
                            &[
                                bin,
                                installation.root.join("usr").join("bin"),
                                Path::new("home").join("bin"),
                            ],
                            "system;tools",
                        ),
                    ),
                ],
                "an existing HOME is left as it is: {prefix}"
            );
        }
    }

    #[test]
    fn an_existing_msystem_is_replaced_and_an_existing_plink_protocol_kept() {
        let installation = Installation::new("ucrt64");
        let msystem = OsString::from("MINGW64");
        let plink = OsString::from("telnet");
        let executable = launch(
            &installation.exec_path("ucrt64"),
            &[("MSYSTEM", &msystem), ("PLINK_PROTOCOL", &plink)],
            None,
        )
        .unwrap();
        assert_eq!(
            variable(&executable, "MSYSTEM"),
            Some(&OsString::from("UCRT64"))
        );
        assert_eq!(variable(&executable, "PLINK_PROTOCOL"), None);
        assert_eq!(
            variable(&executable, "PATH"),
            Some(&search_path(
                &[
                    installation.root.join("ucrt64").join("bin"),
                    installation.root.join("usr").join("bin"),
                ],
                "",
            )),
            "no HOME, nor anywhere to find one, puts no home on PATH"
        );
    }

    #[test]
    fn a_missing_home_is_found_where_the_launcher_finds_it() {
        let installation = Installation::new("mingw64");
        let profile = installation.root.parent().unwrap().join("profile");
        std::fs::create_dir_all(&profile).unwrap();
        // HOMEDRIVE and HOMEPATH name a home together, as `C:` and `\Users\me`.
        let drive = OsString::from(installation.root.parent().unwrap());
        let homepath = OsString::from(format!("{MAIN_SEPARATOR_STR}profile"));
        let missing = OsString::from(format!("{MAIN_SEPARATOR_STR}missing"));
        let user_profile = OsString::from("user-profile");
        let found = |variables: &[(&str, &OsString)], system_directory: Option<&Path>| {
            let executable = launch(
                &installation.exec_path("mingw64"),
                variables,
                system_directory,
            )
            .unwrap();
            let home = variable(&executable, "HOME").cloned();
            let mut entries = vec![
                installation.root.join("mingw64").join("bin"),
                installation.root.join("usr").join("bin"),
            ];
            entries.extend(home.as_ref().map(|home| Path::new(home).join("bin")));
            assert_eq!(
                variable(&executable, "PATH"),
                Some(&search_path(&entries, "")),
                "the home found is the one put on PATH"
            );
            home
        };

        assert_eq!(
            found(
                &[
                    ("HOMEDRIVE", &drive),
                    ("HOMEPATH", &homepath),
                    ("USERPROFILE", &user_profile),
                ],
                None,
            ),
            Some(profile.clone().into_os_string())
        );
        assert_eq!(
            found(
                &[
                    ("HOMEDRIVE", &drive),
                    ("HOMEPATH", &missing),
                    ("USERPROFILE", &user_profile),
                ],
                None,
            ),
            Some(user_profile.clone()),
            "a home share that is not there is passed over"
        );
        let system = PathBuf::from(profile.to_str().unwrap().to_ascii_uppercase());
        assert_eq!(
            found(
                &[
                    ("HOMEDRIVE", &drive),
                    ("HOMEPATH", &homepath),
                    ("USERPROFILE", &user_profile),
                ],
                Some(&system),
            ),
            Some(user_profile.clone()),
            "the system directory, in any case, is never a home"
        );
        assert_eq!(
            found(&[("USERPROFILE", &user_profile)], None),
            Some(user_profile.clone())
        );
        assert_eq!(found(&[], None), None);
    }

    #[test]
    fn msys_options_are_read_from_the_installations_git_bash_config() {
        let installation = Installation::new("mingw64");
        let exec_path = installation.exec_path("mingw64");
        assert_eq!(
            variable(&launch(&exec_path, &[], None).unwrap(), "MSYS"),
            None
        );
        installation.configure("MSYS=enable_pcon\r\nOTHER=1\nMSYS=winsymlinks:nativestrict\n");
        assert_eq!(
            variable(&launch(&exec_path, &[], None).unwrap(), "MSYS"),
            Some(&OsString::from("winsymlinks:nativestrict enable_pcon"))
        );
        let existing = OsString::from("noglob");
        assert_eq!(
            variable(
                &launch(&exec_path, &[("MSYS", &existing)], None).unwrap(),
                "MSYS"
            ),
            Some(&OsString::from(
                "winsymlinks:nativestrict enable_pcon noglob"
            )),
            "each line is put before what is already set"
        );
    }

    #[test]
    fn anything_but_a_git_for_windows_installation_is_left_to_path() {
        let installation = Installation::new("mingw64");
        let root = &installation.root;
        assert!(
            launch(Path::new("mingw64/libexec/git-core"), &[], None).is_none(),
            "a relative exec path"
        );
        // MSYS2's and Cygwin's own Gits keep their commands elsewhere.
        std::fs::create_dir_all(root.join("usr").join("lib").join("git-core")).unwrap();
        assert!(launch(&root.join("usr").join("lib").join("git-core"), &[], None).is_none());
        std::fs::create_dir_all(root.join("usr").join("libexec").join("git-core")).unwrap();
        std::fs::create_dir_all(root.join("usr").join("bin")).unwrap();
        std::fs::write(root.join("usr").join("bin").join("git.exe"), "").unwrap();
        assert!(
            launch(
                &root.join("usr").join("libexec").join("git-core"),
                &[],
                None
            )
            .is_none(),
            "a prefix Git for Windows does not install under"
        );
        let unbuilt = Installation::new("mingw64");
        std::fs::remove_file(unbuilt.root.join("mingw64").join("bin").join("git.exe")).unwrap();
        assert!(
            launch(&unbuilt.exec_path("mingw64"), &[], None).is_none(),
            "no real git.exe to start"
        );
        let msys2 = Installation::new("mingw64");
        std::fs::remove_file(msys2.root.join("cmd").join("git.exe")).unwrap();
        assert!(
            launch(&msys2.exec_path("mingw64"), &[], None).is_none(),
            "no launcher, as with MSYS2's mingw-w64 Git"
        );
        let unreadable = Installation::new("mingw64");
        std::fs::create_dir_all(unreadable.root.join("etc").join("git-bash.config")).unwrap();
        assert!(
            launch(&unreadable.exec_path("mingw64"), &[], None).is_none(),
            "options that cannot be read cannot be applied"
        );
    }
}
