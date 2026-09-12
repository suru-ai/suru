use std::{
    ffi::OsString,
    io,
    process::{Command, Stdio},
};

use super::clipboard::safe_hyperlink_target;

pub(super) fn open(target: &str) -> io::Result<()> {
    let target = safe_hyperlink_target(target)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unsafe hyperlink target"))?;
    let (program, arguments) = opener(target);
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    std::thread::Builder::new()
        .name("suru-hyperlink-opener".to_owned())
        .spawn(move || {
            if let Err(error) = command.status() {
                tracing::warn!("hyperlink opener failed: {error}");
            }
        })
        .map(|_| ())
}

fn opener(target: &str) -> (&'static str, Vec<OsString>) {
    #[cfg(target_os = "windows")]
    {
        (
            "rundll32",
            vec![OsString::from("url.dll,FileProtocolHandler"), target.into()],
        )
    }
    #[cfg(target_os = "macos")]
    {
        ("open", vec![OsString::from("--"), target.into()])
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = if target.starts_with('-') {
            format!("./{target}")
        } else {
            target.to_owned()
        };
        ("xdg-open", vec![target.into()])
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn platform_opener_receives_the_target_as_one_literal_argument() {
        let target = "https://example.test/a?value=$(touch%20nope)&other=two words";
        let (_program, arguments) = super::opener(target);
        assert_eq!(arguments.last().and_then(|arg| arg.to_str()), Some(target));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_xdg_open_receives_no_unsupported_option_separator() {
        let (program, arguments) = super::opener("https://example.test");
        assert_eq!(program, "xdg-open");
        assert_eq!(
            arguments,
            [std::ffi::OsString::from("https://example.test")]
        );

        let (_, relative) = super::opener("-local-file");
        assert_eq!(relative, [std::ffi::OsString::from("./-local-file")]);
    }
}
