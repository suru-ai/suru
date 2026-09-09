//! The deadline every test-harness wait shares.

use std::time::Duration;

/// How long a wait for progress a test expects may take before the test calls
/// it a failure.
///
/// This is a failure deadline, not an expected wait. Every wait bounded by it
/// returns the moment the progress it names arrives, so a generous value costs
/// a passing run nothing; all it decides is how quickly a genuine hang is
/// reported instead of hanging the suite. That makes it worth setting far
/// above what any of these waits needs on an idle machine.
///
/// It needs to be, because the same waits run on Windows under `cargo nextest`,
/// where every test binary runs at once. The paths these tests exercise spawn
/// Git subprocesses — process creation is expensive there, and each spawn is
/// scanned before it runs — so a Session that settles in milliseconds on an
/// idle Linux box can take seconds on a loaded Windows one. A deadline tuned to
/// the idle case reports contention as a protocol failure, and does it in
/// whichever tests happened to lose the race, which is how this looked before:
/// a shifting set of failures that all passed when run alone.
///
/// Waits that prove something does *not* happen are the opposite case. They
/// spend their whole duration on every run, and a generous one would only slow
/// the suite down, so they keep their own short literals and stay out of here.
pub const PROGRESS_DEADLINE: Duration = Duration::from_secs(30);
