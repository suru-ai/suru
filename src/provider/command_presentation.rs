//! Presents the commands Providers report the way a reader should see them,
//! which is why this lives beside the Providers rather than inside one.
//!
//! Codex launches every script through a shell — `/usr/bin/zsh -lc '<script>'`
//! on POSIX systems, `pwsh -Command '<script>'` on Windows — and reports the
//! whole invocation, shlex-joined, as the command. That wrapper is launcher
//! plumbing, not part of the command a reader should see, so a projection
//! strips it before the Activity is recorded. Claude sends the command itself,
//! so for it these shapes only ever appear if the wire drifts. A command in any
//! shape this module does not recognize as plumbing is kept verbatim.
//!
//! Claude also habitually opens its commands with `cd <directory> && `,
//! `cd <directory>; ` or `cd <directory>` on a line of its own, most often into
//! the directory it already works in. A Provider that reports no directory of
//! its own has that change lifted out as the directory the command runs in,
//! leaving the work it was run for as the command.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// A command as a reader should see it, beside the directory it runs in when
/// the command itself says.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct PresentedCommand {
    pub(super) command: String,
    pub(super) cwd: Option<PathBuf>,
}

/// Presents a command a Provider ran, reported without a directory: its
/// launcher wrapper stripped, then any leading changes into absolute
/// directories, each joined to what follows by `&&`, `;` or a new line, lifted
/// out as the directory it runs in. What follows the changes may hold `;`,
/// `||` or new lines. All of it runs in that directory unless a change itself
/// failed, and then the command's output says so; a failed change leaves a
/// Provider's persistent shell wherever it last was, so the rest runs there. A
/// command that may send work to the background is kept whole: `&` runs the
/// change in a subshell of its own, so what follows it runs where the command
/// began however the change went.
pub(super) fn present_command(command: String) -> PresentedCommand {
    present(command, &["&&", ";", "\n"], may_background)
}

/// Presents a command awaiting a person's approval, reported without a
/// directory, as [`present_command`] does — except that the directory is only
/// lifted when the changes and all that follows them are joined by `&&` and
/// pipes. An approval is decided before anything runs, so it names a directory
/// only where every part of the command runs in it or not at all.
pub(super) fn present_command_for_approval(command: String) -> PresentedCommand {
    present(command, &["&&"], |rest| {
        may_background(rest) || rest.contains([';', '\n']) || rest.contains("||")
    })
}

/// Strips the launcher wrapper and lifts the leading changes of directory
/// joined to what follows by one of `joiners`, unless what follows them is a
/// script `keeps_whole` says to keep as it is.
fn present(command: String, joiners: &[&str], keeps_whole: fn(&str) -> bool) -> PresentedCommand {
    let command = strip_launcher_wrapper(command);
    match lift_directory_changes(&command, joiners).filter(|(_, rest)| !keeps_whole(rest)) {
        Some((cwd, rest)) => PresentedCommand {
            command: rest.to_owned(),
            cwd: Some(cwd),
        },
        None => PresentedCommand { command, cwd: None },
    }
}

/// Returns the command as a reader should see it: the script inside a
/// recognized launcher wrapper, or the command verbatim otherwise.
pub(super) fn strip_launcher_wrapper(command: String) -> String {
    let Some(argv) = split_round_trip(&command) else {
        return command;
    };
    match extract_posix_script(&argv).or_else(|| extract_powershell_script(&argv)) {
        Some(script) => script.to_owned(),
        None => command,
    }
}

/// The directory a run of leading `cd <directory>` changes, each followed by
/// one of `joiners`, ends in, and the script after them.
///
/// Only a change into an absolute directory is lifted. A relative one — even
/// after an absolute change — may resolve through `CDPATH`, or against the
/// directory an earlier command left a Provider's persistent shell in, so it
/// stays in the script. `&&` runs what follows in that directory or not at
/// all; after `;` or a new line the rest runs even where the change failed, so
/// a caller accepts them only where the command's output will say it failed.
fn lift_directory_changes<'a>(script: &'a str, joiners: &[&str]) -> Option<(PathBuf, &'a str)> {
    let mut lifted = None;
    let mut rest = script;
    while let Some((directory, after)) = leading_directory_change(rest, joiners) {
        lifted = Some(directory);
        rest = after;
    }
    lifted.map(|directory| (directory, rest))
}

/// Whether `script` holds an `&` that may send work to the background: one
/// that is neither half of `&&` nor part of a redirection (`>&`, `<&`, `&>`)
/// or a `|&` pipe. Quoting is not read for this, so a quoted `&` counts too,
/// since leaving a command whole is always faithful.
///
/// A heredoc's body is data rather than shell, so it is skipped. Where a body
/// lies depends on the shell's quoting, so heredocs are only read up to the
/// first line [`heredocs_opened_by`] cannot read with certainty; every line
/// from there on is scanned as shell. The bodies of the heredocs a line opens
/// follow it in order, and one whose end [`Heredoc::end_of_body`] cannot find
/// counts as may.
fn may_background(script: &str) -> bool {
    let mut lines = script.split('\n');
    let mut reads_heredocs = true;
    while let Some(line) = lines.next() {
        if line_may_background(line) {
            return true;
        }
        let Some(heredocs) = reads_heredocs.then(|| heredocs_opened_by(line)).flatten() else {
            reads_heredocs = false;
            continue;
        };
        for heredoc in heredocs {
            if !heredoc.end_of_body(&mut lines) {
                return true;
            }
        }
    }
    false
}

/// Whether a line holds an `&` that may send work to the background, as
/// [`may_background`] reads it.
fn line_may_background(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes.iter().enumerate().any(|(index, &byte)| {
        let before = index.checked_sub(1).map(|before| bytes[before]);
        let after = bytes.get(index + 1).copied();
        byte == b'&'
            && !matches!(before, Some(b'&' | b'>' | b'<' | b'|'))
            && !matches!(after, Some(b'&' | b'>'))
    })
}

/// The heredocs a line of shell opens, in order, when the line is plain
/// enough to tell: it closes every quote it opens, each delimiter can be read,
/// and it holds nothing that could change how its `<<` or its end is read — no
/// escape, comment, substitution, expansion, subscript or grouping, so none of
/// `\`, `#`, a backquote, `(`, `)`, `{`, `}`, `[` or `]`.
fn heredocs_opened_by(line: &str) -> Option<Vec<Heredoc>> {
    if line.contains(['\\', '#', '`', '(', ')', '{', '}', '[', ']']) {
        return None;
    }
    let mut heredocs = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find(['\'', '"', '<']) {
        let after = &rest[at + 1..];
        rest = match &rest[at..at + 1] {
            "<" if after.starts_with("<<") => &after[2..],
            "<" if after.starts_with('<') => {
                let (heredoc, after_delimiter) = Heredoc::opened_by(&after[1..])?;
                heredocs.push(heredoc);
                after_delimiter
            }
            "<" => after,
            quote => &after[after.find(quote)? + 1..],
        };
    }
    Some(heredocs)
}

/// A heredoc a line of shell opens, whose body follows that line.
struct Heredoc {
    delimiter: String,
    /// Whether it opens with `<<-`, which strips the leading tabs of each
    /// line of the body, delimiter included.
    strips_tabs: bool,
    /// Whether its delimiter is unquoted, so the shell joins a body line
    /// ending in `\` to the next before looking for the delimiter.
    joins_lines: bool,
}

impl Heredoc {
    /// The heredoc a `<<` opens, read from the text after it, and the text
    /// after its delimiter.
    fn opened_by(text: &str) -> Option<(Self, &str)> {
        let (text, strips_tabs) = match text.strip_prefix('-') {
            Some(text) => (text, true),
            None => (text, false),
        };
        let word = text.trim_start_matches([' ', '\t']);
        let (delimiter, after) = literal_word(word)?;
        let quoted = word[..word.len() - after.len()].contains(['\'', '"']);
        (!delimiter.is_empty()).then(|| {
            let heredoc = Self {
                delimiter,
                strips_tabs,
                joins_lines: !quoted,
            };
            (heredoc, after)
        })
    }

    /// Consumes this heredoc's body from the lines following the line that
    /// opened it, through the line that is its delimiter. False when no line
    /// is, or when a line before it is joined to the next, as the shell then
    /// looks for the delimiter in lines this does not join.
    fn end_of_body<'a>(&self, lines: &mut impl Iterator<Item = &'a str>) -> bool {
        for line in lines {
            let content = if self.strips_tabs {
                line.trim_start_matches('\t')
            } else {
                line
            };
            if content == self.delimiter {
                return true;
            }
            if self.joins_lines && line.ends_with('\\') {
                return false;
            }
        }
        false
    }
}

/// The absolute directory a script's leading `cd <directory>` changes into,
/// when one of `joiners` follows it, and the script after that joiner.
fn leading_directory_change<'a>(script: &'a str, joiners: &[&str]) -> Option<(PathBuf, &'a str)> {
    let after_cd = script.strip_prefix("cd")?;
    let target_start = after_cd.trim_start_matches([' ', '\t']);
    if target_start.len() == after_cd.len() {
        return None;
    }
    let (target, after_target) = literal_word(target_start)?;
    let directory = PathBuf::from(target);
    if !directory.is_absolute() {
        return None;
    }
    let after_target = after_target.trim_start_matches([' ', '\t']);
    let rest = joiners
        .iter()
        .find_map(|joiner| after_target.strip_prefix(joiner))?
        // Only the shell's own blanks: any other whitespace is part of a word.
        .trim_start_matches([' ', '\t', '\n']);
    (!rest.is_empty()).then_some((directory, rest))
}

/// The shell word opening `text` when it expands to nothing but itself, and
/// the text after it. Quoting is resolved; a word the shell would expand —
/// variables, substitutions, globs, braces, escapes — is refused.
fn literal_word(text: &str) -> Option<(String, &str)> {
    let mut word = String::new();
    let mut chars = text.char_indices();
    let end = loop {
        let Some((index, character)) = chars.next() else {
            break text.len();
        };
        match character {
            '\'' => loop {
                match chars.next()?.1 {
                    '\'' => break,
                    quoted => word.push(quoted),
                }
            },
            '"' => loop {
                match chars.next()?.1 {
                    '"' => break,
                    '$' | '`' | '\\' => return None,
                    quoted => word.push(quoted),
                }
            },
            ' ' | '\t' | '\n' | '&' | ';' | '|' | '<' | '>' | '(' | ')' => break index,
            '$' | '`' | '\\' | '*' | '?' | '[' | '{' => return None,
            bare => word.push(bare),
        }
    };
    Some((word, &text[end..]))
}

/// Splits the shlex-joined command back into the argv Codex joined, refusing
/// when splitting is lossy: quoting shlex cannot parse, or a split that would
/// not faithfully re-join (as with Windows path escapes).
fn split_round_trip(command: &str) -> Option<Vec<String>> {
    let argv = shlex::split(command)?;
    let round_trip = shlex::try_join(argv.iter().map(String::as_str)).ok()?;
    // `:\` marks Windows drive or UNC paths, whose backslashes a POSIX split
    // reads as escapes; a re-join can then look faithful while the argv is
    // mangled, so only an exact round trip is trusted for them.
    if round_trip == command
        || (!command.contains(":\\") && shlex::split(&round_trip).as_ref() == Some(&argv))
    {
        Some(argv)
    } else {
        None
    }
}

/// The script inside a POSIX shell wrapper: exactly `<shell> -lc|-c <script>`
/// where the shell's basename is `bash`, `zsh`, or `sh`.
fn extract_posix_script(argv: &[String]) -> Option<&str> {
    let [shell, flag, script] = argv else {
        return None;
    };
    (matches!(flag.as_str(), "-lc" | "-c") && is_posix_shell(shell)).then_some(script)
}

fn is_posix_shell(shell: &str) -> bool {
    shell_name(shell).is_some_and(|name| matches!(name, "bash" | "zsh" | "sh"))
}

/// The script inside a PowerShell wrapper: `pwsh` or `powershell` followed by
/// a run of launcher flags with the script right after `-Command`/`-c`. Any
/// flag outside that run means the invocation is not Codex's plumbing.
fn extract_powershell_script(argv: &[String]) -> Option<&str> {
    let [shell, flags @ ..] = argv else {
        return None;
    };
    if !shell_name(shell).is_some_and(|name| matches!(name, "pwsh" | "powershell")) {
        return None;
    }
    let mut flags = flags.iter();
    while let Some(flag) = flags.next() {
        if flag.eq_ignore_ascii_case("-Command") || flag.eq_ignore_ascii_case("-c") {
            return flags.next().map(String::as_str);
        }
        if !flag.eq_ignore_ascii_case("-NoLogo") && !flag.eq_ignore_ascii_case("-NoProfile") {
            return None;
        }
    }
    None
}

/// The name a shell is recognized by: the last path component with every
/// extension stripped, as Codex's own shell detection resolves it.
fn shell_name(shell: &str) -> Option<&str> {
    let mut name = Path::new(shell).file_name().and_then(OsStr::to_str)?;
    while let Some(stem) = Path::new(name).file_stem().and_then(OsStr::to_str) {
        if stem == name {
            break;
        }
        name = stem;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absolute directory on the platform running the test, since `Path::is_absolute` is
    /// platform-defined.
    fn root() -> &'static str {
        if cfg!(windows) { "C:/srv" } else { "/srv" }
    }

    fn presented(command: &str) -> (String, Option<PathBuf>) {
        let PresentedCommand { command, cwd } = present_command(command.to_owned());
        (command, cwd)
    }

    #[test]
    fn leading_changes_into_absolute_directories_become_the_directory_the_command_runs_in() {
        let root = root();
        for (command, expected_command, expected_cwd) in [
            (
                format!("cd {root}/app && cargo check"),
                "cargo check",
                format!("{root}/app"),
            ),
            (format!("cd {root}/app&&ls"), "ls", format!("{root}/app")),
            (
                format!("cd '{root}/my app' && ls"),
                "ls",
                format!("{root}/my app"),
            ),
            (
                format!("cd \"{root}/my app\" && ls"),
                "ls",
                format!("{root}/my app"),
            ),
            (
                format!("cd {root}/a && cd {root}/b && git status && git log"),
                "git status && git log",
                format!("{root}/b"),
            ),
            (
                format!("cd {root}/a && cd sub && ls"),
                "cd sub && ls",
                format!("{root}/a"),
            ),
            (
                format!("cd {root}/app && cargo test 2>&1 | tail -5"),
                "cargo test 2>&1 | tail -5",
                format!("{root}/app"),
            ),
            (
                format!("cd {root}/app && cargo build &> build.log |& tee"),
                "cargo build &> build.log |& tee",
                format!("{root}/app"),
            ),
            (
                format!("cd {root}/app && git status; git log -1 || true"),
                "git status; git log -1 || true",
                format!("{root}/app"),
            ),
            (format!("cd {root}/app; ls"), "ls", format!("{root}/app")),
            (
                format!("cd {root}/.suru-worktrees/x; ls; cat CONTEXT.md"),
                "ls; cat CONTEXT.md",
                format!("{root}/.suru-worktrees/x"),
            ),
            (
                format!("cd {root}/a; cd {root}/b && ls"),
                "ls",
                format!("{root}/b"),
            ),
            (format!("cd {root}/app\nls"), "ls", format!("{root}/app")),
            (
                format!("cd {root}/a\n\u{a0}cd {root}/b\npwd"),
                &format!("\u{a0}cd {root}/b\npwd"),
                format!("{root}/a"),
            ),
            (
                format!("cd {root}/a\n\rcd {root}/b\npwd"),
                &format!("\rcd {root}/b\npwd"),
                format!("{root}/a"),
            ),
            (
                format!("cd {root}/a \ncd {root}/b; git status\ngit log"),
                "git status\ngit log",
                format!("{root}/b"),
            ),
        ] {
            assert_eq!(
                presented(&command),
                (
                    expected_command.to_owned(),
                    Some(PathBuf::from(expected_cwd))
                ),
                "{command}"
            );
        }
    }

    #[test]
    fn a_change_of_directory_suru_cannot_resolve_or_that_may_not_hold_stays_in_the_command() {
        let root = root();
        for command in [
            "cd src && ls".to_owned(),
            format!("cd {root}/app || ls"),
            format!("cd {root}/app"),
            format!("cd {root}/app && "),
            format!("cd {root}/$DIR && ls"),
            format!("cd \"{root}/$DIR\" && ls"),
            format!("cd {root}/ap* && ls"),
            format!("cd '{root}/app && ls"),
            format!("cd -P {root}/app && ls"),
            format!("cd {root}/app ls && ls"),
            format!("cdx {root}/app && ls"),
            format!("echo cd {root}/app && ls"),
            format!("cd {root}/app && echo work & wait; pwd"),
            format!("cd {root}/app && cargo run &"),
            format!("cd {root}/app && cat <<EOF &\nbody\nEOF"),
            format!("cd {root}/app && cat <<EOF\na & b"),
            format!("cd {root}/app && cat <<EOF\nbody\nEOF\nsleep 1 &"),
            format!("cd {root}/app && cat <<A <<B\nbody\nA\na & b"),
            format!("cd {root}/app && cat <<'EOF\na & b\nEOF"),
            format!("cd {root}/app && cat <<$END\na & b\n$END"),
            format!("cd {root}/app && cat <<<'a & b' &"),
            format!("cd {root}/app && echo '<<X'\nsleep 1 & pwd\nX"),
            format!("cd {root}/app && echo \"<<X\n\" &\npwd\nX"),
            format!("cd {root}/app && true # <<X\nsleep 1 & pwd\nX"),
            format!("cd {root}/app && echo $((1<<X))\nsleep 1 & pwd\nX"),
            format!("cd {root}/app && echo ${{a:-<<X}}\nsleep 1 & pwd\nX"),
            format!("cd {root}/app && echo ${{a:-${{b:-x}}<<X\n}} &\npwd\nX"),
            format!("cd {root}/app && echo \"a\\&b\""),
            format!("cd {root}/app && a[1<<true ]=x &&\ntrue & pwd\ntrue"),
            format!("cd {root}/app && cat <<EOF $(true\n) & pwd\nEOF"),
            format!("cd {root}/app && x=$(cat <<'EOF'\na & b\nEOF\n)"),
            format!("cd {root}/app && cat <<EOF \\\n& pwd\nbody\nEOF"),
            format!("cd {root}/app && cat <<EOF \"a\nb\" &\nbody\nEOF"),
            format!("cd {root}/app && cat <<true\ntr\\\nue\nsleep 1 & pwd\ntrue"),
        ] {
            assert_eq!(presented(&command), (command.clone(), None), "{command}");
        }
    }

    #[test]
    fn an_ampersand_in_a_heredoc_body_is_data_that_sends_nothing_to_the_background() {
        let root = root();
        for script in [
            "python3 - <<'EOF'\nx = a & b\nEOF",
            "cat <<-\"EOF\"\n\tx & y\n\tEOF\necho done",
            "cat <<EOF\n&\nEOF\nls",
            "cat <<A << B\n&\nA\n&\nB\nls",
            "cat <<EOF 2>&1 | tail\n&\nEOF",
            "cat <<'EOF'\na & \\\nEOF",
            "cat <<EOF | grep '<<X'\n&\nEOF",
        ] {
            let command = format!("cd {root}/app && {script}");
            assert_eq!(
                presented(&command),
                (
                    script.to_owned(),
                    Some(PathBuf::from(format!("{root}/app")))
                ),
                "{command}"
            );
        }
    }

    #[test]
    fn an_approval_names_the_directory_only_where_every_part_of_the_command_runs_in_it() {
        let root = root();
        for command in ["cargo test 2>&1 | tail -5", "cargo fmt && cargo check"] {
            let PresentedCommand {
                command: lifted,
                cwd,
            } = present_command_for_approval(format!("cd {root}/app && {command}"));
            assert_eq!(
                (lifted.as_str(), cwd),
                (command, Some(PathBuf::from(format!("{root}/app"))))
            );
        }
        for command in [
            format!("cd {root}/app && git status; git log"),
            format!("cd {root}/app && cargo test || true"),
            format!("cd {root}/app && git status\ngit log"),
            format!("cd {root}/app && cargo run & wait"),
            format!("cd {root}/app; ls"),
            format!("cd {root}/app\nls"),
            format!("cd {root}/app || ls"),
            format!("cd {root}/app && python3 - <<'EOF'\nx = a & b\nEOF"),
        ] {
            assert_eq!(
                present_command_for_approval(command.clone()),
                PresentedCommand {
                    command: command.clone(),
                    cwd: None,
                },
                "{command}"
            );
        }
    }

    #[test]
    fn a_change_of_directory_inside_a_launcher_wrapper_is_lifted_once_the_wrapper_is_stripped() {
        let root = root();
        let wrapped = shlex::try_join(["bash", "-lc", &format!("cd {root}/app && ls")])
            .expect("the wrapper joins");
        assert_eq!(
            presented(&wrapped),
            ("ls".to_owned(), Some(PathBuf::from(format!("{root}/app"))))
        );
    }
}
