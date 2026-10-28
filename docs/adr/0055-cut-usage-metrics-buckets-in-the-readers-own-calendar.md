# Cut Usage Metrics buckets in the reader's own calendar

Usage Metrics divide what a Server's Sessions consumed into time buckets — a day, an hour, a month — and a day only has edges in some time zone. The Server and the reader need not share one: a Client turned toward a Remote reads a machine that may keep another zone, or none worth trusting. We decided the reader cuts the buckets. A Usage Metrics query names its buckets as half-open intervals of Session timestamps, earliest first and none overlapping, and the Server places each Turn in whichever interval holds it, knowing nothing of calendars or zones. The TUI's metrics calendar turns a range, the moment now, and the local zone into those intervals: a pure function, so its clock and zone are injected and tests name their zones rather than borrowing the host's.

A day is cut at its first moment rather than at midnight, which a transition can skip, so a day daylight saving shortens to 23 hours or lengthens to 25 is still one bucket. The local zone is read through `jiff`, which finds the system zone on Windows, macOS, and Linux and bundles the time zone database on Windows, which keeps none; the `time` crate's local offset cannot be read reliably in a multithreaded process. The zone is read again at each read rather than once at startup, so a host that moves zones is followed.

## Considered Options

- **The Server cuts the buckets in its own zone.** Rejected: a remote Server's day would be drawn as the reader's, and a reader travelling with a laptop would see its own Server's days shift under it.
- **The reader sends its zone and the Server cuts the buckets in it.** Rejected: the Server would need a time zone database of its own and to agree with the reader's on every rule, and a query could no longer say plainly which spans it asked for.
- **Fixed UTC buckets, relabelled by the reader.** Rejected: a UTC day is not the reader's day anywhere but on the meridian, so "today" would split across two bars.

## Consequences

- A remote Server's Usage Metrics are read in the reader's local days, as its own Server's are.
- A query's bucket count is bounded (`MAX_USAGE_METRICS_BUCKETS`), and so is every range the calendar cuts; the bound is the Server's to enforce, since any client may ask.
- The Server stays calendar-free: a new range or bucket size is a change to the calendar alone.
