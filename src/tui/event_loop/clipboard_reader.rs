//! Reading the host clipboard for a paste, off the UI thread.
//!
//! A lazy worker owns the native source, created, used, and dropped on its own
//! thread as the copy worker owns its writer, and answers each read in the
//! order it was asked. An image comes first, encoded as PNG on this thread so
//! the UI thread never touches pixels; text comes next; anything else reads as
//! empty.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::tui::ClipboardRead;

const DEFAULT_EXIT_TIMEOUT: Duration = Duration::from_millis(250);

/// What one native reading of the host clipboard offers. Either question may
/// answer that the clipboard holds nothing of its kind.
pub(super) trait NativeClipboardSource {
    fn image(&mut self) -> Result<Option<NativeImage>, String>;
    fn text(&mut self) -> Result<Option<String>, String>;
}

/// An image as a native clipboard holds it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum NativeImage {
    /// Straight RGBA pixels, row by row.
    Pixels {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// An image already encoded in some format.
    Encoded(Vec<u8>),
}

/// What a paste takes from `source`: an image before text, and the reason
/// the clipboard could not be read only where neither could be.
pub(super) fn read_clipboard(source: &mut (impl NativeClipboardSource + ?Sized)) -> ClipboardRead {
    let image_error = match source.image() {
        Ok(Some(image)) => return encode(image),
        Ok(None) => None,
        Err(error) => Some(error),
    };
    match source.text() {
        Ok(Some(text)) => ClipboardRead::Text(text),
        Ok(None) => image_error.map_or(ClipboardRead::Empty, |reason| ClipboardRead::Failed {
            reason,
        }),
        Err(error) => ClipboardRead::Failed {
            reason: image_error.unwrap_or(error),
        },
    }
}

const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

fn encode(image: NativeImage) -> ClipboardRead {
    match image {
        NativeImage::Pixels {
            width,
            height,
            rgba,
        } => encode_png(width, height, &rgba).map_or_else(
            |reason| ClipboardRead::Failed { reason },
            |png| ClipboardRead::Image { png },
        ),
        NativeImage::Encoded(png) if png.starts_with(PNG_SIGNATURE) => ClipboardRead::Image { png },
        NativeImage::Encoded(bytes) => ClipboardRead::Unsupported {
            format: format_name(&bytes).to_owned(),
        },
    }
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    use image::ImageEncoder as _;

    let expected = u64::from(width) * u64::from(height) * 4;
    if rgba.len() as u64 != expected {
        return Err(format!(
            "the clipboard image's {} bytes do not fill {width}×{height} pixels",
            rgba.len()
        ));
    }
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|error| format!("the clipboard image could not be encoded as PNG: {error}"))?;
    Ok(png)
}

/// The name an encoded image's format is known by, from its first bytes.
fn format_name(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\xff\xd8\xff") {
        "JPEG"
    } else if bytes.starts_with(b"GIF8") {
        "GIF"
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        "WebP"
    } else if bytes.starts_with(b"BM") {
        "BMP"
    } else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        "TIFF"
    } else {
        "unrecognized"
    }
}

/// The host clipboard as arboard reaches it, opened on first use.
#[derive(Default)]
pub(super) struct ArboardSource {
    clipboard: Option<arboard::Clipboard>,
}

impl ArboardSource {
    fn clipboard(&mut self) -> Result<&mut arboard::Clipboard, String> {
        if self.clipboard.is_none() {
            self.clipboard = Some(arboard::Clipboard::new().map_err(|error| error.to_string())?);
        }
        Ok(self
            .clipboard
            .as_mut()
            .expect("the clipboard was just opened"))
    }
}

impl NativeClipboardSource for ArboardSource {
    fn image(&mut self) -> Result<Option<NativeImage>, String> {
        match self.clipboard()?.get_image() {
            Ok(image) => Ok(Some(NativeImage::Pixels {
                width: u32::try_from(image.width).map_err(|error| error.to_string())?,
                height: u32::try_from(image.height).map_err(|error| error.to_string())?,
                rgba: image.bytes.into_owned(),
            })),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn text(&mut self) -> Result<Option<String>, String> {
        match self.clipboard()?.get_text() {
            Ok(text) => Ok(Some(text)),
            Err(arboard::Error::ContentNotAvailable) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
}

/// The source a production client reads its clipboard through: arboard, and
/// under WSL the Windows clipboard's image besides.
pub(super) fn native_clipboard_source() -> Box<dyn NativeClipboardSource> {
    #[cfg(target_os = "linux")]
    if wsl::is_wsl() {
        return Box::new(wsl::ImageFallback::new(
            ArboardSource::default(),
            wsl::PowerShellClipboard::new(),
        ));
    }
    Box::new(ArboardSource::default())
}

type ReadAnswered = Box<dyn FnOnce(ClipboardRead) + Send>;

type SourceFactory = dyn Fn() -> Box<dyn NativeClipboardSource> + Send + Sync;

#[derive(Default)]
struct PendingReads {
    reads: VecDeque<ReadAnswered>,
    stopping: bool,
}

struct Worker {
    pending: Arc<(Mutex<PendingReads>, Condvar)>,
    thread: JoinHandle<()>,
}

/// The worker that reads the host clipboard for pastes.
pub(super) struct ClipboardReader {
    create_source: Arc<SourceFactory>,
    worker: Option<Worker>,
    exit_timeout: Duration,
}

impl ClipboardReader {
    pub(super) fn new(
        create_source: impl Fn() -> Box<dyn NativeClipboardSource> + Send + Sync + 'static,
    ) -> Self {
        Self {
            create_source: Arc::new(create_source),
            worker: None,
            exit_timeout: DEFAULT_EXIT_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(super) fn with_exit_timeout(mut self, timeout: Duration) -> Self {
        self.exit_timeout = timeout;
        self
    }

    /// Reads the clipboard on the worker, answering once the read is done.
    pub(super) fn read(&mut self, answered: impl FnOnce(ClipboardRead) + Send + 'static) {
        if self.worker.is_none() {
            match self.start() {
                Ok(worker) => self.worker = Some(worker),
                Err(error) => {
                    tracing::warn!(%error, "could not start the clipboard reading thread");
                    answered(ClipboardRead::Failed {
                        reason: format!("the clipboard reading thread could not start: {error}"),
                    });
                    return;
                }
            }
        }
        let (slot, wake) = &*self.worker.as_ref().unwrap().pending;
        slot.lock().unwrap().reads.push_back(Box::new(answered));
        wake.notify_one();
    }

    fn start(&self) -> std::io::Result<Worker> {
        let pending = Arc::new((Mutex::new(PendingReads::default()), Condvar::new()));
        let incoming = pending.clone();
        let create_source = self.create_source.clone();
        let thread = thread::Builder::new()
            .name("suru-clipboard-read".to_owned())
            .spawn(move || {
                // Created, used, and dropped here: a native source need not
                // even be Send.
                let mut source = create_source();
                loop {
                    let answered = {
                        let (slot, wake) = &*incoming;
                        let mut pending = slot.lock().unwrap();
                        while pending.reads.is_empty() && !pending.stopping {
                            pending = wake.wait(pending).unwrap();
                        }
                        if pending.stopping {
                            break;
                        }
                        pending.reads.pop_front().expect("a read is waiting")
                    };
                    // Never hold the queue's lock across a native read.
                    answered(read_clipboard(source.as_mut()));
                }
            })?;
        Ok(Worker { pending, thread })
    }

    /// Stops the worker, giving a read in progress a bounded chance to end.
    /// A stalled worker is detached when the bound expires; reads still
    /// waiting are never answered.
    pub(super) fn shutdown(&mut self) -> bool {
        let began = Instant::now();
        let Some(worker) = self.worker.take() else {
            return true;
        };
        let (slot, wake) = &*worker.pending;
        slot.lock().unwrap().stopping = true;
        wake.notify_one();
        while !worker.thread.is_finished() {
            let remaining = self.exit_timeout.saturating_sub(began.elapsed());
            if remaining.is_zero() {
                tracing::debug!("clipboard reading thread exceeded its exit timeout");
                return false;
            }
            thread::sleep(remaining.min(Duration::from_millis(1)));
        }
        worker.thread.join().is_ok()
    }
}

impl Drop for ClipboardReader {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Under WSL, arboard reaches the Linux side's clipboard, which never holds
/// the image a reader copied in Windows. Codex's TUI asks Windows PowerShell
/// for it instead; so does Suru, but only once arboard found no image.
#[cfg(target_os = "linux")]
mod wsl {
    use std::ffi::OsString;
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use base64::{Engine as _, engine::general_purpose::STANDARD};

    use super::{NativeClipboardSource, NativeImage};

    /// How long PowerShell has to answer before a paste stops waiting on it.
    /// A cold `powershell.exe` launched across the WSL boundary takes a second
    /// or two.
    const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

    /// Writes the Windows clipboard's image to standard output as base64 PNG,
    /// and exits 1 when it holds none.
    const SCRIPT: &str = "$image = Get-Clipboard -Format Image; \
        if ($image -eq $null) { exit 1 }; \
        $stream = New-Object System.IO.MemoryStream; \
        $image.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png); \
        [Console]::Out.Write([Convert]::ToBase64String($stream.ToArray()))";

    /// Whether this Linux is WSL, from its environment or its kernel's name.
    pub(super) fn is_wsl() -> bool {
        looks_like_wsl(
            |name| std::env::var_os(name).is_some(),
            std::fs::read_to_string("/proc/version").ok().as_deref(),
        )
    }

    fn looks_like_wsl(is_set: impl Fn(&str) -> bool, kernel_version: Option<&str>) -> bool {
        is_set("WSL_DISTRO_NAME")
            || is_set("WSL_INTEROP")
            || kernel_version.is_some_and(|version| {
                let version = version.to_lowercase();
                version.contains("microsoft") || version.contains("wsl")
            })
    }

    /// A source whose image, when it has none, is asked of `fallback` instead.
    /// Text is always the primary source's.
    pub(super) struct ImageFallback<P, F> {
        primary: P,
        fallback: F,
    }

    impl<P, F> ImageFallback<P, F> {
        pub(super) fn new(primary: P, fallback: F) -> Self {
            Self { primary, fallback }
        }
    }

    impl<P: NativeClipboardSource, F: NativeClipboardSource> NativeClipboardSource
        for ImageFallback<P, F>
    {
        fn image(&mut self) -> Result<Option<NativeImage>, String> {
            let primary = match self.primary.image() {
                Ok(Some(image)) => return Ok(Some(image)),
                other => other,
            };
            match self.fallback.image() {
                Ok(Some(image)) => Ok(Some(image)),
                Ok(None) => primary,
                Err(error) => {
                    // The fallback is a guess about the host; its failing
                    // must not stand in the way of the text pasting.
                    tracing::debug!(%error, "the Windows clipboard could not be read");
                    primary
                }
            }
        }

        fn text(&mut self) -> Result<Option<String>, String> {
            self.primary.text()
        }
    }

    /// The Windows clipboard's image, as Windows PowerShell hands it over.
    pub(super) struct PowerShellClipboard {
        program: OsString,
        args: Vec<OsString>,
        timeout: Duration,
    }

    impl PowerShellClipboard {
        pub(super) fn new() -> Self {
            Self {
                program: "powershell.exe".into(),
                args: ["-NoProfile", "-NonInteractive", "-Command", SCRIPT]
                    .map(OsString::from)
                    .to_vec(),
                timeout: DEFAULT_TIMEOUT,
            }
        }

        #[cfg(test)]
        fn command(program: &str, args: &[&str]) -> Self {
            Self {
                program: program.into(),
                args: args.iter().map(OsString::from).collect(),
                timeout: DEFAULT_TIMEOUT,
            }
        }

        #[cfg(test)]
        fn with_timeout(mut self, timeout: Duration) -> Self {
            self.timeout = timeout;
            self
        }

        fn run(&self) -> Result<Option<Vec<u8>>, String> {
            let program = self.program.to_string_lossy();
            let mut child = Command::new(&self.program)
                .args(&self.args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| format!("{program} could not start: {error}"))?;
            let mut stdout = child.stdout.take().expect("standard output is piped");
            // Drained beside the wait, so an image larger than the pipe's
            // buffer cannot stall the process that is writing it.
            let output = thread::spawn(move || {
                let mut bytes = Vec::new();
                stdout.read_to_end(&mut bytes).map(|_| bytes)
            });
            let deadline = Instant::now() + self.timeout;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if Instant::now() >= deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!(
                            "{program} did not answer within {:?}",
                            self.timeout
                        ));
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(5)),
                    Err(error) => return Err(format!("{program} could not be awaited: {error}")),
                }
            };
            if !status.success() {
                return Ok(None);
            }
            let output = output
                .join()
                .map_err(|_| format!("{program}'s output could not be read"))?
                .map_err(|error| format!("{program}'s output could not be read: {error}"))?;
            let encoded = String::from_utf8_lossy(&output);
            let encoded = encoded.trim();
            if encoded.is_empty() {
                return Ok(None);
            }
            STANDARD
                .decode(encoded)
                .map(Some)
                .map_err(|error| format!("{program}'s image could not be decoded: {error}"))
        }
    }

    impl NativeClipboardSource for PowerShellClipboard {
        fn image(&mut self) -> Result<Option<NativeImage>, String> {
            Ok(self.run()?.map(NativeImage::Encoded))
        }

        fn text(&mut self) -> Result<Option<String>, String> {
            Ok(None)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::tui::ClipboardRead;

        use super::super::{PNG_SIGNATURE, read_clipboard, tests::Scripted};

        #[test]
        fn wsl_is_recognized_by_its_environment_or_its_kernel() {
            let unset = |_: &str| false;
            assert!(!looks_like_wsl(unset, None));
            assert!(!looks_like_wsl(unset, Some("Linux version 6.9.0-arch1-1")));
            assert!(looks_like_wsl(
                unset,
                Some("Linux version 5.15.153.1-microsoft-standard-WSL2")
            ));
            assert!(looks_like_wsl(|name| name == "WSL_DISTRO_NAME", None));
            assert!(looks_like_wsl(|name| name == "WSL_INTEROP", None));
        }

        #[test]
        fn the_windows_image_is_asked_for_only_when_the_native_clipboard_has_none() {
            let windows_png = [PNG_SIGNATURE, b"windows"].concat();

            let mut source = ImageFallback::new(
                Scripted::text("native text"),
                Scripted::encoded(windows_png.clone()),
            );
            assert_eq!(
                read_clipboard(&mut source),
                ClipboardRead::Image {
                    png: windows_png.clone()
                }
            );

            let mut source = ImageFallback::new(
                Scripted::encoded([PNG_SIGNATURE, b"native"].concat()),
                Scripted::unreachable(),
            );
            assert_eq!(
                read_clipboard(&mut source),
                ClipboardRead::Image {
                    png: [PNG_SIGNATURE, b"native"].concat()
                }
            );
        }

        #[test]
        fn a_windows_clipboard_without_an_image_or_unreadable_leaves_the_native_text() {
            let mut source = ImageFallback::new(Scripted::text("native text"), Scripted::empty());
            assert_eq!(
                read_clipboard(&mut source),
                ClipboardRead::Text("native text".to_owned())
            );

            let mut source = ImageFallback::new(
                Scripted::text("native text"),
                Scripted::failing("powershell.exe could not start"),
            );
            assert_eq!(
                read_clipboard(&mut source),
                ClipboardRead::Text("native text".to_owned())
            );

            let mut source = ImageFallback::new(Scripted::empty(), Scripted::empty());
            assert_eq!(read_clipboard(&mut source), ClipboardRead::Empty);
        }

        #[test]
        fn powershell_hands_over_its_image_as_base64_and_exits_one_without_one() {
            let png = [PNG_SIGNATURE, b"pixels"].concat();
            let script = format!("printf '%s' '{}'", STANDARD.encode(&png));
            let mut windows = PowerShellClipboard::command("sh", &["-c", &script]);
            assert_eq!(windows.image(), Ok(Some(NativeImage::Encoded(png))));

            let mut windows = PowerShellClipboard::command("sh", &["-c", "exit 1"]);
            assert_eq!(windows.image(), Ok(None));

            let mut windows = PowerShellClipboard::command("suru-no-such-powershell", &[]);
            assert!(windows.image().is_err());
        }

        #[test]
        fn powershell_that_does_not_answer_in_time_is_stopped() {
            let began = Instant::now();
            let mut windows = PowerShellClipboard::command("sh", &["-c", "sleep 5"])
                .with_timeout(Duration::from_millis(50));
            let error = windows.image().expect_err("the read times out");
            assert!(error.contains("did not answer"), "{error}");
            assert!(
                began.elapsed() < Duration::from_secs(2),
                "{:?}",
                began.elapsed()
            );
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A clipboard that answers as scripted, and never touches a real one.
    pub(in crate::tui::event_loop) struct Scripted {
        image: Result<Option<NativeImage>, String>,
        text: Result<Option<String>, String>,
        reads: Option<mpsc::Sender<thread::ThreadId>>,
    }

    impl Scripted {
        fn new(
            image: Result<Option<NativeImage>, String>,
            text: Result<Option<String>, String>,
        ) -> Self {
            Self {
                image,
                text,
                reads: None,
            }
        }

        pub(in crate::tui::event_loop) fn empty() -> Self {
            Self::new(Ok(None), Ok(None))
        }

        pub(in crate::tui::event_loop) fn text(text: &str) -> Self {
            Self::new(Ok(None), Ok(Some(text.to_owned())))
        }

        pub(in crate::tui::event_loop) fn encoded(bytes: Vec<u8>) -> Self {
            Self::new(Ok(Some(NativeImage::Encoded(bytes))), Ok(None))
        }

        pub(in crate::tui::event_loop) fn failing(reason: &str) -> Self {
            Self::new(Err(reason.to_owned()), Err(reason.to_owned()))
        }

        /// A source a test expects never to be asked.
        #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
        pub(in crate::tui::event_loop) fn unreachable() -> Self {
            Self::new(
                Err("this source must not be read".to_owned()),
                Err("this source must not be read".to_owned()),
            )
        }
    }

    impl NativeClipboardSource for Scripted {
        fn image(&mut self) -> Result<Option<NativeImage>, String> {
            if let Some(reads) = &self.reads {
                let _ = reads.send(thread::current().id());
            }
            self.image.clone()
        }

        fn text(&mut self) -> Result<Option<String>, String> {
            self.text.clone()
        }
    }

    fn pixels(width: u32, height: u32) -> NativeImage {
        NativeImage::Pixels {
            width,
            height,
            rgba: (0..width * height * 4).map(|byte| byte as u8).collect(),
        }
    }

    #[test]
    fn clipboard_pixels_are_encoded_as_a_png_of_the_same_size() {
        let mut source = Scripted::new(Ok(Some(pixels(3, 2))), Ok(Some("ignored".to_owned())));
        let ClipboardRead::Image { png } = read_clipboard(&mut source) else {
            panic!("an image reads as an image");
        };
        assert!(png.starts_with(PNG_SIGNATURE));
        let decoder = image::codecs::png::PngDecoder::new(std::io::Cursor::new(&png))
            .expect("the PNG decodes");
        assert_eq!(image::ImageDecoder::dimensions(&decoder), (3, 2));
    }

    #[test]
    fn pixels_that_do_not_fill_their_size_fail_the_read() {
        let mut source = Scripted::new(
            Ok(Some(NativeImage::Pixels {
                width: 4,
                height: 4,
                rgba: vec![0; 7],
            })),
            Ok(None),
        );
        assert!(matches!(
            read_clipboard(&mut source),
            ClipboardRead::Failed { reason } if reason.contains("4×4")
        ));
    }

    #[test]
    fn an_encoded_image_passes_as_png_or_is_named_as_unsupported() {
        let png = [PNG_SIGNATURE, b"rest"].concat();
        assert_eq!(
            read_clipboard(&mut Scripted::encoded(png.clone())),
            ClipboardRead::Image { png }
        );
        for (bytes, format) in [
            (b"BM\x00\x00".to_vec(), "BMP"),
            (b"II*\x00rest".to_vec(), "TIFF"),
            (b"\xff\xd8\xff\xe0".to_vec(), "JPEG"),
            (b"RIFF\0\0\0\0WEBPVP8 ".to_vec(), "WebP"),
            (b"not an image".to_vec(), "unrecognized"),
        ] {
            assert_eq!(
                read_clipboard(&mut Scripted::encoded(bytes)),
                ClipboardRead::Unsupported {
                    format: format.to_owned()
                }
            );
        }
    }

    #[test]
    fn text_pastes_when_there_is_no_image_and_nothing_reads_as_empty() {
        assert_eq!(
            read_clipboard(&mut Scripted::text("hello")),
            ClipboardRead::Text("hello".to_owned())
        );
        assert_eq!(read_clipboard(&mut Scripted::empty()), ClipboardRead::Empty);
    }

    #[test]
    fn a_clipboard_that_cannot_be_read_says_why_unless_its_text_could_be() {
        assert_eq!(
            read_clipboard(&mut Scripted::failing("held by another party")),
            ClipboardRead::Failed {
                reason: "held by another party".to_owned()
            }
        );
        let mut source = Scripted::new(Err("image unreadable".to_owned()), Ok(Some("text".into())));
        assert_eq!(
            read_clipboard(&mut source),
            ClipboardRead::Text("text".to_owned())
        );
        let mut source = Scripted::new(Err("image unreadable".to_owned()), Ok(None));
        assert_eq!(
            read_clipboard(&mut source),
            ClipboardRead::Failed {
                reason: "image unreadable".to_owned()
            }
        );
    }

    #[test]
    fn reads_are_answered_in_order_on_one_worker_that_owns_the_source() {
        let (reads, read_on) = mpsc::channel();
        let (created, created_on) = mpsc::channel();
        let mut reader = ClipboardReader::new(move || {
            created.send(thread::current().id()).unwrap();
            let mut source = Scripted::text("pasted");
            source.reads = Some(reads.clone());
            Box::new(source)
        })
        .with_exit_timeout(Duration::from_millis(100));
        let (answers, answered) = mpsc::channel();
        for index in 0..3 {
            let answers = answers.clone();
            reader.read(move |read| answers.send((index, read)).unwrap());
        }
        for index in 0..3 {
            assert_eq!(
                answered.recv_timeout(Duration::from_secs(1)).unwrap(),
                (index, ClipboardRead::Text("pasted".to_owned()))
            );
        }
        let worker = created_on.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_ne!(worker, thread::current().id());
        assert!(
            created_on.try_recv().is_err(),
            "one source serves every read"
        );
        assert_eq!(read_on.try_iter().collect::<Vec<_>>(), vec![worker; 3]);
        assert!(reader.shutdown());
    }

    #[test]
    fn a_run_without_a_paste_never_opens_the_clipboard() {
        let mut reader = ClipboardReader::new(|| -> Box<dyn NativeClipboardSource> {
            panic!("a run without pasting must not open a clipboard");
        })
        .with_exit_timeout(Duration::ZERO);
        assert!(reader.shutdown());
    }

    #[test]
    fn shutdown_bounds_a_stalled_read() {
        struct Stalled(Arc<Mutex<mpsc::Receiver<()>>>, mpsc::Sender<()>);

        impl NativeClipboardSource for Stalled {
            fn image(&mut self) -> Result<Option<NativeImage>, String> {
                self.1.send(()).unwrap();
                let _ = self.0.lock().unwrap().recv();
                Ok(None)
            }

            fn text(&mut self) -> Result<Option<String>, String> {
                Ok(None)
            }
        }

        let (release, resume) = mpsc::channel();
        let resume = Arc::new(Mutex::new(resume));
        let (started, reading) = mpsc::channel();
        let mut reader = ClipboardReader::new(move || {
            Box::new(Stalled(resume.clone(), started.clone())) as Box<dyn NativeClipboardSource>
        })
        .with_exit_timeout(Duration::from_millis(20));
        reader.read(|_| {});
        reading.recv_timeout(Duration::from_secs(1)).unwrap();
        let began = Instant::now();
        assert!(!reader.shutdown());
        assert!(
            began.elapsed() < Duration::from_millis(120),
            "{:?}",
            began.elapsed()
        );
        release.send(()).unwrap();
    }
}
