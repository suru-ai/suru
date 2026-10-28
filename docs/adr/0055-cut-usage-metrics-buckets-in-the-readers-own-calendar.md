# Cut Usage Metrics buckets in the reader's own calendar

Usage Metrics divide what a Server's Sessions consumed into time buckets — a day, an hour, a month — and a day only has edges in some time zone. The Server and the reader need not share one: a Client turned toward a Remote reads a machine that may keep another zone, or none worth trusting. We decided the reader cuts the buckets. A Usage Metrics query names its buckets as half-open intervals of Session timestamps, earliest first and none overlapping, and the Server places each Turn in whichever interval holds it, knowing nothing of calendars or zones. The TUI's metrics calendar turns a range, the moment now, and the local zone into those intervals: a pure function, so its clock and zone are injected and tests name their zones rather than borrowing the host's.

A day is cut at its first moment rather than at midnight, which a transition can skip, so a day daylight saving shortens to 23 hours or lengthens to 25 is still one bucket. The local zone is read through `jiff`, which finds the system zone on Windows, macOS, and Linux and bundles the time zone database on Windows, which keeps none; the `time` crate's local offset cannot be read reliably in a multithreaded process. The zone is read again at each read rather than once at startup, so a host that moves zones is followed.

Each range is cut in that calendar. Today is its local hours, cut an elapsed hour apart from its first moment, so a day daylight saving shortens has 23 and one it lengthens has 25, the repeated hour a bucket of its own. Seven and thirty days are local days; Year is the current calendar month and the eleven before it. All time runs by the month from the month of the earliest timed Turn, by the calendar quarter once that would be more than 36 months, and by the calendar year once it would be more than twelve years' worth — never more years than a query may ask for, the latest kept. Where All time is asked for before any report has said when the earliest Turn was, the reader first asks for no buckets at all, a read that answers with that instant alone, and then cuts its buckets from it; a report that names a Turn earlier than the buckets it answered reach is read once more from there, and no more, so a cut that cannot reach back that far never loops.

The calendar cuts the interval a range is compared with too, and the query names it beside the buckets as one more span of Session timestamps, which the Server answers with that span's figures taken together; it may overlap the buckets, since it is counted apart from them. A range is compared with the period of its own length before it, measured in the reader's calendar and taken as far as the range has run: the range moved back by as many local days or calendar months as it spans, ending at the same reading of the clock as many days or months before now. Today is compared with yesterday up to this time of day, seven and thirty days with the seven or thirty local days before as far into the last of them, and Year with the twelve calendar months before as far into the twelfth, a day the shorter month lacks taken as its last. So a range that has run fourteen and a half hours into its last day is never set against a whole one. All time is compared with nothing: no period comes before it.

## Considered Options

- **The Server cuts the buckets in its own zone.** Rejected: a remote Server's day would be drawn as the reader's, and a reader travelling with a laptop would see its own Server's days shift under it.
- **The reader sends its zone and the Server cuts the buckets in it.** Rejected: the Server would need a time zone database of its own and to agree with the reader's on every rule, and a query could no longer say plainly which spans it asked for.
- **Fixed UTC buckets, relabelled by the reader.** Rejected: a UTC day is not the reader's day anywhere but on the meridian, so "today" would split across two bars.
- **A separate route for the earliest Turn.** Rejected: a query with no buckets already asks for exactly that, so All time's first read is the one read the Server has.
- **Compare with the whole period before.** Rejected: Today at nine in the morning would always fall short of all of yesterday, and every range would read as falling until its last bucket ended.
- **Compare with the same elapsed time immediately before the range.** Rejected: Today's comparison would be yesterday evening, set against this morning, and a day daylight saving changed would shift every edge off the reader's calendar.
- **The Server works out the comparison from the buckets.** Rejected: it would need the calendar the reader keeps, which is the reader's alone.
- **All time stretched to a fixed number of bars.** Rejected: a bar of 1.7 months is in no one's calendar; months, quarters, and years are, and the 36-month and twelve-year thresholds keep the bar count within what a terminal can draw.

## Consequences

- A remote Server's Usage Metrics are read in the reader's local days, as its own Server's are.
- A query's bucket count is bounded (`MAX_USAGE_METRICS_BUCKETS`), and so is every range the calendar cuts; the bound is the Server's to enforce, since any client may ask.
- The Server stays calendar-free: a new range or bucket size is a change to the calendar alone.
- A comparison costs no read of its own: it travels in the query the range already makes.
- All time with nothing yet read costs two reads, the first of them answering almost nothing; any report since the view opened, whatever its range, spares the first.
