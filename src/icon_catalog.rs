//! The Suru-owned Icon Catalog: the set of Nerd Font glyphs Suru knows by
//! name, from which every Icon is chosen — by the Title Errand that derives a
//! Session's Icon alongside its Title, or by the user through the Icon
//! Picker. See ADR 0028 for why the Catalog is Suru's own hand-written table
//! rather than the whole of Nerd Fonts, and why an Icon is carried as this
//! table's name rather than as a codepoint.
//!
//! Every entry's codepoint is pinned in `tests` against Nerd Fonts' published
//! `glyphnames.json`, checked in under `tests/fixtures`, so a typo here fails
//! the build rather than drawing as a missing glyph.
//!
//! Glyphs come mostly from Material Design (`md-*`) for a consistent stroke
//! style, with Devicons (`dev-*`) and Seti (`seti-*`) filling in the
//! languages, frameworks, and tools that Material Design has no glyph for.

// The Catalog itself is unused outside its own tests until Title derivation,
// the transcript header, and the Icon Picker (issue #358 and later) start
// reading Icons through it.
#![allow(dead_code)]

use std::sync::LazyLock;

/// Which half of the Icon Catalog one [`IconEntry`] belongs to. The Icon
/// Picker's grid groups by this before anything else, so a glyph standing for
/// what a Session's work is about never sits beside one standing for what it
/// is written in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IconGroup {
    /// What a Session's or Workspace's work is about: a bug, a feature, a
    /// test, and the like.
    Subject,
    /// What the work is written in or runs on: a language, a framework, a
    /// platform.
    Technology,
}

/// One glyph the Icon Catalog offers, addressable by its `name` — the form an
/// Icon is derived, stored, and carried over the protocol as.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IconEntry {
    /// The Catalog name, e.g. `md-bug` or `dev-rust`. Stable once shipped: a
    /// stored Icon that no longer resolves draws as absent rather than as
    /// something else, so renaming an entry is a retirement, not an edit.
    pub(crate) name: &'static str,
    /// The Nerd Font codepoint this name resolves to, pinned by
    /// [`tests::every_codepoint_is_pinned_to_the_nerd_fonts_fixture`] against
    /// the fixture in `tests/fixtures/glyphnames.json`.
    pub(crate) glyph: char,
    /// Which half of the Catalog this entry belongs to.
    pub(crate) group: IconGroup,
    /// Lowercase words [`search`] matches against, in addition to `name`
    /// itself.
    pub(crate) keywords: &'static [&'static str],
}

/// The Icon Catalog's entries, in the Icon Picker's deliberate order: every
/// Subject before every Technology, and within each group the order below —
/// curated by hand rather than sorted, so the most generally useful glyphs of
/// each kind sit up front.
static CATALOG: &[IconEntry] = &[
    IconEntry {
        name: "md-bug",
        glyph: '\u{f00e4}',
        group: IconGroup::Subject,
        keywords: &["bug", "defect", "fix", "error"],
    },
    IconEntry {
        name: "md-bug_check",
        glyph: '\u{f0a2e}',
        group: IconGroup::Subject,
        keywords: &["bug", "fixed", "verified", "qa"],
    },
    IconEntry {
        name: "md-creation",
        glyph: '\u{f0674}',
        group: IconGroup::Subject,
        keywords: &["feature", "sparkle", "new", "magic"],
    },
    IconEntry {
        name: "md-flask",
        glyph: '\u{f0093}',
        group: IconGroup::Subject,
        keywords: &["test", "experiment", "science", "lab"],
    },
    IconEntry {
        name: "md-test_tube",
        glyph: '\u{f0668}',
        group: IconGroup::Subject,
        keywords: &["test", "experiment", "qa", "lab"],
    },
    IconEntry {
        name: "md-book_open_variant",
        glyph: '\u{f14f7}',
        group: IconGroup::Subject,
        keywords: &["docs", "documentation", "book", "read"],
    },
    IconEntry {
        name: "md-book_open_page_variant",
        glyph: '\u{f05da}',
        group: IconGroup::Subject,
        keywords: &["docs", "documentation", "manual", "guide"],
    },
    IconEntry {
        name: "md-database",
        glyph: '\u{f01bc}',
        group: IconGroup::Subject,
        keywords: &["database", "data", "storage", "sql"],
    },
    IconEntry {
        name: "md-database_outline",
        glyph: '\u{f1632}',
        group: IconGroup::Subject,
        keywords: &["database", "schema", "data", "sql"],
    },
    IconEntry {
        name: "md-lock",
        glyph: '\u{f033e}',
        group: IconGroup::Subject,
        keywords: &["security", "lock", "private", "auth"],
    },
    IconEntry {
        name: "md-shield_check",
        glyph: '\u{f0565}',
        group: IconGroup::Subject,
        keywords: &["security", "shield", "verified", "safe"],
    },
    IconEntry {
        name: "md-shield_lock",
        glyph: '\u{f099d}',
        group: IconGroup::Subject,
        keywords: &["security", "shield", "lock", "protect"],
    },
    IconEntry {
        name: "md-shield_alert",
        glyph: '\u{f0ecc}',
        group: IconGroup::Subject,
        keywords: &["security", "threat", "risk", "warning"],
    },
    IconEntry {
        name: "md-shield_bug",
        glyph: '\u{f13da}',
        group: IconGroup::Subject,
        keywords: &["security", "vulnerability", "bug", "audit"],
    },
    IconEntry {
        name: "md-speedometer",
        glyph: '\u{f04c5}',
        group: IconGroup::Subject,
        keywords: &["performance", "speed", "gauge", "benchmark"],
    },
    IconEntry {
        name: "md-gauge",
        glyph: '\u{f029a}',
        group: IconGroup::Subject,
        keywords: &["performance", "gauge", "metric", "measure"],
    },
    IconEntry {
        name: "md-recycle_variant",
        glyph: '\u{f139d}',
        group: IconGroup::Subject,
        keywords: &["refactor", "cleanup", "recycle", "reuse"],
    },
    IconEntry {
        name: "md-source_branch",
        glyph: '\u{f062c}',
        group: IconGroup::Subject,
        keywords: &["branch", "git", "version", "vcs"],
    },
    IconEntry {
        name: "md-network",
        glyph: '\u{f06f3}',
        group: IconGroup::Subject,
        keywords: &["network", "connectivity", "topology", "link"],
    },
    IconEntry {
        name: "md-access_point_network",
        glyph: '\u{f0002}',
        group: IconGroup::Subject,
        keywords: &["network", "wifi", "connectivity", "wireless"],
    },
    IconEntry {
        name: "md-console",
        glyph: '\u{f018d}',
        group: IconGroup::Subject,
        keywords: &["terminal", "console", "shell", "cli"],
    },
    IconEntry {
        name: "md-chart_bar",
        glyph: '\u{f0128}',
        group: IconGroup::Subject,
        keywords: &["chart", "analytics", "stats", "metrics"],
    },
    IconEntry {
        name: "md-chart_line",
        glyph: '\u{f012a}',
        group: IconGroup::Subject,
        keywords: &["chart", "trend", "analytics", "graph"],
    },
    IconEntry {
        name: "md-chart_pie",
        glyph: '\u{f012b}',
        group: IconGroup::Subject,
        keywords: &["chart", "analytics", "pie", "stats"],
    },
    IconEntry {
        name: "md-email",
        glyph: '\u{f01ee}',
        group: IconGroup::Subject,
        keywords: &["mail", "email", "message", "inbox"],
    },
    IconEntry {
        name: "md-calendar",
        glyph: '\u{f00ed}',
        group: IconGroup::Subject,
        keywords: &["calendar", "schedule", "date", "planning"],
    },
    IconEntry {
        name: "md-rocket_launch",
        glyph: '\u{f14de}',
        group: IconGroup::Subject,
        keywords: &["launch", "release", "ship", "deploy"],
    },
    IconEntry {
        name: "md-puzzle",
        glyph: '\u{f0431}',
        group: IconGroup::Subject,
        keywords: &["integration", "plugin", "feature", "piece"],
    },
    IconEntry {
        name: "md-wrench",
        glyph: '\u{f05b7}',
        group: IconGroup::Subject,
        keywords: &["tool", "fix", "maintenance", "config"],
    },
    IconEntry {
        name: "md-hammer_wrench",
        glyph: '\u{f1323}',
        group: IconGroup::Subject,
        keywords: &["build", "tool", "maintenance", "fix"],
    },
    IconEntry {
        name: "md-cog",
        glyph: '\u{f0493}',
        group: IconGroup::Subject,
        keywords: &["settings", "config", "gear", "options"],
    },
    IconEntry {
        name: "md-key_variant",
        glyph: '\u{f030b}',
        group: IconGroup::Subject,
        keywords: &["auth", "key", "access", "credential"],
    },
    IconEntry {
        name: "md-cloud",
        glyph: '\u{f015f}',
        group: IconGroup::Subject,
        keywords: &["cloud", "hosting", "infrastructure", "remote"],
    },
    IconEntry {
        name: "md-package_variant_closed",
        glyph: '\u{f03d7}',
        group: IconGroup::Subject,
        keywords: &["package", "dependency", "module", "box"],
    },
    IconEntry {
        name: "md-layers_triple",
        glyph: '\u{f0f58}',
        group: IconGroup::Subject,
        keywords: &["architecture", "layers", "stack", "structure"],
    },
    IconEntry {
        name: "md-magnify",
        glyph: '\u{f0349}',
        group: IconGroup::Subject,
        keywords: &["search", "find", "investigate", "lookup"],
    },
    IconEntry {
        name: "md-filter_variant",
        glyph: '\u{f0236}',
        group: IconGroup::Subject,
        keywords: &["filter", "query", "narrow", "search"],
    },
    IconEntry {
        name: "md-flag_checkered",
        glyph: '\u{f023c}',
        group: IconGroup::Subject,
        keywords: &["milestone", "finish", "release", "goal"],
    },
    IconEntry {
        name: "md-star_four_points",
        glyph: '\u{f0ae2}',
        group: IconGroup::Subject,
        keywords: &["feature", "sparkle", "highlight", "new"],
    },
    IconEntry {
        name: "md-heart_pulse",
        glyph: '\u{f05f6}',
        group: IconGroup::Subject,
        keywords: &["health", "monitoring", "vitals", "uptime"],
    },
    IconEntry {
        name: "md-trophy",
        glyph: '\u{f0538}',
        group: IconGroup::Subject,
        keywords: &["achievement", "milestone", "win", "success"],
    },
    IconEntry {
        name: "md-target",
        glyph: '\u{f04fe}',
        group: IconGroup::Subject,
        keywords: &["goal", "objective", "target", "focus"],
    },
    IconEntry {
        name: "md-compass",
        glyph: '\u{f018b}',
        group: IconGroup::Subject,
        keywords: &["navigation", "direction", "explore", "guide"],
    },
    IconEntry {
        name: "md-map_marker",
        glyph: '\u{f034e}',
        group: IconGroup::Subject,
        keywords: &["location", "map", "marker", "place"],
    },
    IconEntry {
        name: "md-folder_multiple",
        glyph: '\u{f0253}',
        group: IconGroup::Subject,
        keywords: &["organize", "folders", "files", "structure"],
    },
    IconEntry {
        name: "md-file_document",
        glyph: '\u{f0219}',
        group: IconGroup::Subject,
        keywords: &["document", "file", "text", "paper"],
    },
    IconEntry {
        name: "md-clipboard_check",
        glyph: '\u{f014e}',
        group: IconGroup::Subject,
        keywords: &["checklist", "task", "review", "done"],
    },
    IconEntry {
        name: "md-alert_circle",
        glyph: '\u{f0028}',
        group: IconGroup::Subject,
        keywords: &["alert", "warning", "issue", "attention"],
    },
    IconEntry {
        name: "md-check_circle",
        glyph: '\u{f05e0}',
        group: IconGroup::Subject,
        keywords: &["done", "complete", "success", "verified"],
    },
    IconEntry {
        name: "md-close_circle",
        glyph: '\u{f0159}',
        group: IconGroup::Subject,
        keywords: &["failed", "error", "cancel", "reject"],
    },
    IconEntry {
        name: "md-information",
        glyph: '\u{f02fc}',
        group: IconGroup::Subject,
        keywords: &["info", "information", "about", "details"],
    },
    IconEntry {
        name: "md-help_circle",
        glyph: '\u{f02d7}',
        group: IconGroup::Subject,
        keywords: &["help", "question", "support", "faq"],
    },
    IconEntry {
        name: "md-trending_up",
        glyph: '\u{f0535}',
        group: IconGroup::Subject,
        keywords: &["growth", "improvement", "trend", "progress"],
    },
    IconEntry {
        name: "md-sync",
        glyph: '\u{f04e6}',
        group: IconGroup::Subject,
        keywords: &["sync", "refresh", "update", "reload"],
    },
    IconEntry {
        name: "md-history",
        glyph: '\u{f02da}',
        group: IconGroup::Subject,
        keywords: &["history", "log", "timeline", "past"],
    },
    IconEntry {
        name: "md-backup_restore",
        glyph: '\u{f006f}',
        group: IconGroup::Subject,
        keywords: &["backup", "restore", "recovery", "undo"],
    },
    IconEntry {
        name: "md-tune",
        glyph: '\u{f062e}',
        group: IconGroup::Subject,
        keywords: &["settings", "tuning", "adjust", "configure"],
    },
    IconEntry {
        name: "md-palette",
        glyph: '\u{f03d8}',
        group: IconGroup::Subject,
        keywords: &["design", "ui", "theme", "style"],
    },
    IconEntry {
        name: "md-format_paint",
        glyph: '\u{f027c}',
        group: IconGroup::Subject,
        keywords: &["design", "style", "theme", "paint"],
    },
    IconEntry {
        name: "md-lightbulb_on",
        glyph: '\u{f06e8}',
        group: IconGroup::Subject,
        keywords: &["idea", "insight", "brainstorm", "concept"],
    },
    IconEntry {
        name: "md-format_list_checks",
        glyph: '\u{f0756}',
        group: IconGroup::Subject,
        keywords: &["checklist", "todo", "tasks", "list"],
    },
    IconEntry {
        name: "md-pencil",
        glyph: '\u{f03eb}',
        group: IconGroup::Subject,
        keywords: &["edit", "write", "draft", "update"],
    },
    IconEntry {
        name: "md-message_text",
        glyph: '\u{f0369}',
        group: IconGroup::Subject,
        keywords: &["chat", "message", "discussion", "comment"],
    },
    IconEntry {
        name: "md-bell",
        glyph: '\u{f009a}',
        group: IconGroup::Subject,
        keywords: &["notification", "alert", "reminder", "bell"],
    },
    IconEntry {
        name: "md-archive",
        glyph: '\u{f003c}',
        group: IconGroup::Subject,
        keywords: &["archive", "storage", "backup", "old"],
    },
    IconEntry {
        name: "md-trash_can",
        glyph: '\u{f0a79}',
        group: IconGroup::Subject,
        keywords: &["delete", "remove", "cleanup", "trash"],
    },
    IconEntry {
        name: "md-link_variant",
        glyph: '\u{f0339}',
        group: IconGroup::Subject,
        keywords: &["link", "connect", "url", "reference"],
    },
    IconEntry {
        name: "md-web",
        glyph: '\u{f059f}',
        group: IconGroup::Subject,
        keywords: &["web", "website", "internet", "browser"],
    },
    IconEntry {
        name: "md-account_group",
        glyph: '\u{f0849}',
        group: IconGroup::Subject,
        keywords: &["team", "collaboration", "people", "users"],
    },
    IconEntry {
        name: "md-cellphone",
        glyph: '\u{f011c}',
        group: IconGroup::Subject,
        keywords: &["mobile", "phone", "app", "device"],
    },
    IconEntry {
        name: "md-monitor",
        glyph: '\u{f0379}',
        group: IconGroup::Subject,
        keywords: &["desktop", "display", "screen", "ui"],
    },
    IconEntry {
        name: "md-server",
        glyph: '\u{f048b}',
        group: IconGroup::Subject,
        keywords: &["server", "backend", "infrastructure", "host"],
    },
    IconEntry {
        name: "md-server_network",
        glyph: '\u{f048d}',
        group: IconGroup::Subject,
        keywords: &["infrastructure", "distributed", "cluster", "network"],
    },
    IconEntry {
        name: "md-timer_sand",
        glyph: '\u{f051f}',
        group: IconGroup::Subject,
        keywords: &["waiting", "pending", "loading", "timer"],
    },
    IconEntry {
        name: "md-clock_fast",
        glyph: '\u{f0152}',
        group: IconGroup::Subject,
        keywords: &["speed", "fast", "quick", "time"],
    },
    IconEntry {
        name: "md-file_document_edit",
        glyph: '\u{f0dc8}',
        group: IconGroup::Subject,
        keywords: &["edit", "draft", "revision", "document"],
    },
    IconEntry {
        name: "md-file_compare",
        glyph: '\u{f08aa}',
        group: IconGroup::Subject,
        keywords: &["diff", "compare", "review", "changes"],
    },
    IconEntry {
        name: "md-file_tree",
        glyph: '\u{f0645}',
        group: IconGroup::Subject,
        keywords: &["structure", "tree", "hierarchy", "files"],
    },
    IconEntry {
        name: "md-graph",
        glyph: '\u{f1049}',
        group: IconGroup::Subject,
        keywords: &["graph", "diagram", "relationships", "nodes"],
    },
    IconEntry {
        name: "md-source_repository",
        glyph: '\u{f0ccf}',
        group: IconGroup::Subject,
        keywords: &["repository", "repo", "source", "code"],
    },
    IconEntry {
        name: "md-source_pull",
        glyph: '\u{f04c2}',
        group: IconGroup::Subject,
        keywords: &["pull request", "review", "merge", "vcs"],
    },
    IconEntry {
        name: "md-source_merge",
        glyph: '\u{f062d}',
        group: IconGroup::Subject,
        keywords: &["merge", "git", "branch", "combine"],
    },
    IconEntry {
        name: "md-source_commit",
        glyph: '\u{f0718}',
        group: IconGroup::Subject,
        keywords: &["commit", "git", "history", "change"],
    },
    IconEntry {
        name: "md-bank",
        glyph: '\u{f0070}',
        group: IconGroup::Subject,
        keywords: &["finance", "billing", "payment", "accounting"],
    },
    IconEntry {
        name: "md-microphone",
        glyph: '\u{f036c}',
        group: IconGroup::Subject,
        keywords: &["voice", "audio", "speech", "mic"],
    },
    IconEntry {
        name: "md-camera",
        glyph: '\u{f0100}',
        group: IconGroup::Subject,
        keywords: &["media", "photo", "capture", "screenshot"],
    },
    IconEntry {
        name: "md-video",
        glyph: '\u{f0567}',
        group: IconGroup::Subject,
        keywords: &["video", "media", "recording", "stream"],
    },
    IconEntry {
        name: "md-image",
        glyph: '\u{f02e9}',
        group: IconGroup::Subject,
        keywords: &["image", "media", "picture", "asset"],
    },
    IconEntry {
        name: "md-music_note",
        glyph: '\u{f0387}',
        group: IconGroup::Subject,
        keywords: &["audio", "music", "sound", "media"],
    },
    IconEntry {
        name: "md-gift",
        glyph: '\u{f0e44}',
        group: IconGroup::Subject,
        keywords: &["release", "bonus", "new", "surprise"],
    },
    IconEntry {
        name: "md-robot",
        glyph: '\u{f06a9}',
        group: IconGroup::Subject,
        keywords: &["automation", "ai", "bot", "agent"],
    },
    IconEntry {
        name: "md-brain",
        glyph: '\u{f09d1}',
        group: IconGroup::Subject,
        keywords: &["ai", "intelligence", "thinking", "cognition"],
    },
    IconEntry {
        name: "md-anchor",
        glyph: '\u{f0031}',
        group: IconGroup::Subject,
        keywords: &["stability", "anchor", "fixed", "base"],
    },
    IconEntry {
        name: "md-eye",
        glyph: '\u{f0208}',
        group: IconGroup::Subject,
        keywords: &["review", "watch", "observe", "monitor"],
    },
    IconEntry {
        name: "md-wheelchair_accessibility",
        glyph: '\u{f05a4}',
        group: IconGroup::Subject,
        keywords: &["accessibility", "a11y", "inclusive"],
    },
    IconEntry {
        name: "md-translate",
        glyph: '\u{f05ca}',
        group: IconGroup::Subject,
        keywords: &["translate", "i18n", "localization", "language"],
    },
    IconEntry {
        name: "md-printer",
        glyph: '\u{f042a}',
        group: IconGroup::Subject,
        keywords: &["printer", "print", "hardware"],
    },
    IconEntry {
        name: "md-keyboard",
        glyph: '\u{f030c}',
        group: IconGroup::Subject,
        keywords: &["keyboard", "input", "hardware"],
    },
    IconEntry {
        name: "md-mouse",
        glyph: '\u{f037d}',
        group: IconGroup::Subject,
        keywords: &["mouse", "input", "hardware"],
    },
    IconEntry {
        name: "md-api",
        glyph: '\u{f109b}',
        group: IconGroup::Subject,
        keywords: &["api", "interface", "endpoint"],
    },
    IconEntry {
        name: "md-webhook",
        glyph: '\u{f062f}',
        group: IconGroup::Subject,
        keywords: &["webhook", "event", "callback"],
    },
    IconEntry {
        name: "md-cached",
        glyph: '\u{f00e8}',
        group: IconGroup::Subject,
        keywords: &["cache", "caching", "speed"],
    },
    IconEntry {
        name: "md-memory",
        glyph: '\u{f035b}',
        group: IconGroup::Subject,
        keywords: &["memory", "ram", "hardware"],
    },
    IconEntry {
        name: "md-chip",
        glyph: '\u{f061a}',
        group: IconGroup::Subject,
        keywords: &["chip", "hardware", "embedded"],
    },
    IconEntry {
        name: "md-cpu_64_bit",
        glyph: '\u{f0ee0}',
        group: IconGroup::Subject,
        keywords: &["cpu", "processor", "hardware"],
    },
    IconEntry {
        name: "md-bluetooth",
        glyph: '\u{f00af}',
        group: IconGroup::Subject,
        keywords: &["bluetooth", "wireless", "device"],
    },
    IconEntry {
        name: "md-wifi",
        glyph: '\u{f05a9}',
        group: IconGroup::Subject,
        keywords: &["wifi", "wireless", "network"],
    },
    IconEntry {
        name: "md-lan",
        glyph: '\u{f0317}',
        group: IconGroup::Subject,
        keywords: &["lan", "network", "ethernet"],
    },
    IconEntry {
        name: "md-cart_outline",
        glyph: '\u{f0111}',
        group: IconGroup::Subject,
        keywords: &["cart", "shopping", "commerce"],
    },
    IconEntry {
        name: "md-currency_usd",
        glyph: '\u{f01c1}',
        group: IconGroup::Subject,
        keywords: &["currency", "billing", "payment", "price"],
    },
    IconEntry {
        name: "md-receipt",
        glyph: '\u{f0449}',
        group: IconGroup::Subject,
        keywords: &["receipt", "billing", "invoice"],
    },
    IconEntry {
        name: "md-forum",
        glyph: '\u{f028c}',
        group: IconGroup::Subject,
        keywords: &["forum", "discussion", "community"],
    },
    IconEntry {
        name: "md-comment_text",
        glyph: '\u{f0188}',
        group: IconGroup::Subject,
        keywords: &["comment", "discussion", "feedback"],
    },
    IconEntry {
        name: "md-tag_outline",
        glyph: '\u{f04fc}',
        group: IconGroup::Subject,
        keywords: &["tag", "label", "category"],
    },
    IconEntry {
        name: "md-bookmark",
        glyph: '\u{f00c0}',
        group: IconGroup::Subject,
        keywords: &["bookmark", "save", "favorite"],
    },
    IconEntry {
        name: "md-table",
        glyph: '\u{f04eb}',
        group: IconGroup::Subject,
        keywords: &["table", "grid", "data"],
    },
    IconEntry {
        name: "md-file_table",
        glyph: '\u{f0c7e}',
        group: IconGroup::Subject,
        keywords: &["spreadsheet", "table", "data"],
    },
    IconEntry {
        name: "md-form_select",
        glyph: '\u{f1401}',
        group: IconGroup::Subject,
        keywords: &["form", "input", "select"],
    },
    IconEntry {
        name: "md-login",
        glyph: '\u{f0342}',
        group: IconGroup::Subject,
        keywords: &["login", "signin", "auth"],
    },
    IconEntry {
        name: "md-logout",
        glyph: '\u{f0343}',
        group: IconGroup::Subject,
        keywords: &["logout", "signout", "auth"],
    },
    IconEntry {
        name: "md-account",
        glyph: '\u{f0004}',
        group: IconGroup::Subject,
        keywords: &["account", "profile", "user"],
    },
    IconEntry {
        name: "md-badge_account",
        glyph: '\u{f0da7}',
        group: IconGroup::Subject,
        keywords: &["badge", "identity", "account"],
    },
    IconEntry {
        name: "md-certificate",
        glyph: '\u{f0124}',
        group: IconGroup::Subject,
        keywords: &["certificate", "credential", "tls"],
    },
    IconEntry {
        name: "md-school",
        glyph: '\u{f0474}',
        group: IconGroup::Subject,
        keywords: &["learning", "tutorial", "education"],
    },
    IconEntry {
        name: "md-math_compass",
        glyph: '\u{f0358}',
        group: IconGroup::Subject,
        keywords: &["math", "geometry", "calculation"],
    },
    IconEntry {
        name: "md-function_variant",
        glyph: '\u{f0871}',
        group: IconGroup::Subject,
        keywords: &["function", "variable", "math"],
    },
    IconEntry {
        name: "md-code_braces",
        glyph: '\u{f0169}',
        group: IconGroup::Subject,
        keywords: &["code", "syntax", "braces"],
    },
    IconEntry {
        name: "md-code_tags",
        glyph: '\u{f0174}',
        group: IconGroup::Subject,
        keywords: &["code", "markup", "tags"],
    },
    IconEntry {
        name: "md-regex",
        glyph: '\u{f0451}',
        group: IconGroup::Subject,
        keywords: &["regex", "pattern", "matching"],
    },
    IconEntry {
        name: "md-xml",
        glyph: '\u{f05c0}',
        group: IconGroup::Subject,
        keywords: &["xml", "markup", "data"],
    },
    IconEntry {
        name: "md-application",
        glyph: '\u{f08c6}',
        group: IconGroup::Subject,
        keywords: &["application", "app", "software"],
    },
    IconEntry {
        name: "md-window_restore",
        glyph: '\u{f05b2}',
        group: IconGroup::Subject,
        keywords: &["window", "ui", "desktop"],
    },
    IconEntry {
        name: "md-view_dashboard",
        glyph: '\u{f056e}',
        group: IconGroup::Subject,
        keywords: &["dashboard", "overview", "ui"],
    },
    IconEntry {
        name: "md-theme_light_dark",
        glyph: '\u{f050e}',
        group: IconGroup::Subject,
        keywords: &["theme", "dark mode", "appearance"],
    },
    IconEntry {
        name: "md-brush",
        glyph: '\u{f00e3}',
        group: IconGroup::Subject,
        keywords: &["design", "brush", "art"],
    },
    IconEntry {
        name: "md-ruler",
        glyph: '\u{f046d}',
        group: IconGroup::Subject,
        keywords: &["measure", "ruler", "layout"],
    },
    IconEntry {
        name: "md-motion_play",
        glyph: '\u{f158f}',
        group: IconGroup::Subject,
        keywords: &["animation", "motion", "transition"],
    },
    IconEntry {
        name: "md-play_circle",
        glyph: '\u{f040c}',
        group: IconGroup::Subject,
        keywords: &["play", "start", "run"],
    },
    IconEntry {
        name: "md-pause_circle",
        glyph: '\u{f03e5}',
        group: IconGroup::Subject,
        keywords: &["pause", "stop", "hold"],
    },
    IconEntry {
        name: "md-upload",
        glyph: '\u{f0552}',
        group: IconGroup::Subject,
        keywords: &["upload", "transfer", "publish"],
    },
    IconEntry {
        name: "md-download",
        glyph: '\u{f01da}',
        group: IconGroup::Subject,
        keywords: &["download", "transfer", "fetch"],
    },
    IconEntry {
        name: "md-export",
        glyph: '\u{f0207}',
        group: IconGroup::Subject,
        keywords: &["export", "output", "transfer"],
    },
    IconEntry {
        name: "md-import",
        glyph: '\u{f02fa}',
        group: IconGroup::Subject,
        keywords: &["import", "input", "transfer"],
    },
    IconEntry {
        name: "md-cloud_sync",
        glyph: '\u{f063f}',
        group: IconGroup::Subject,
        keywords: &["sync", "cloud", "backup"],
    },
    IconEntry {
        name: "md-earth",
        glyph: '\u{f01e7}',
        group: IconGroup::Subject,
        keywords: &["global", "world", "internationalization"],
    },
    IconEntry {
        name: "md-satellite_variant",
        glyph: '\u{f0471}',
        group: IconGroup::Subject,
        keywords: &["satellite", "telemetry", "gps"],
    },
    IconEntry {
        name: "md-home",
        glyph: '\u{f02dc}',
        group: IconGroup::Subject,
        keywords: &["home", "dashboard", "landing"],
    },
    IconEntry {
        name: "md-factory",
        glyph: '\u{f020f}',
        group: IconGroup::Subject,
        keywords: &["factory", "production", "manufacturing"],
    },
    IconEntry {
        name: "md-gamepad_variant",
        glyph: '\u{f0297}',
        group: IconGroup::Subject,
        keywords: &["game", "gaming", "controller"],
    },
    IconEntry {
        name: "dev-rust",
        glyph: '\u{e7a8}',
        group: IconGroup::Technology,
        keywords: &["rust", "systems", "language"],
    },
    IconEntry {
        name: "dev-python",
        glyph: '\u{e73c}',
        group: IconGroup::Technology,
        keywords: &["python", "scripting", "language"],
    },
    IconEntry {
        name: "dev-go",
        glyph: '\u{e724}',
        group: IconGroup::Technology,
        keywords: &["go", "golang", "language"],
    },
    IconEntry {
        name: "dev-java",
        glyph: '\u{e738}',
        group: IconGroup::Technology,
        keywords: &["java", "jvm", "language"],
    },
    IconEntry {
        name: "dev-javascript",
        glyph: '\u{e781}',
        group: IconGroup::Technology,
        keywords: &["javascript", "js", "language"],
    },
    IconEntry {
        name: "dev-typescript",
        glyph: '\u{e8ca}',
        group: IconGroup::Technology,
        keywords: &["typescript", "ts", "language"],
    },
    IconEntry {
        name: "dev-ruby",
        glyph: '\u{e739}',
        group: IconGroup::Technology,
        keywords: &["ruby", "language"],
    },
    IconEntry {
        name: "dev-php",
        glyph: '\u{e73d}',
        group: IconGroup::Technology,
        keywords: &["php", "language"],
    },
    IconEntry {
        name: "dev-csharp",
        glyph: '\u{e7b2}',
        group: IconGroup::Technology,
        keywords: &["csharp", "c#", "dotnet", "language"],
    },
    IconEntry {
        name: "dev-cplusplus",
        glyph: '\u{e7a3}',
        group: IconGroup::Technology,
        keywords: &["cplusplus", "c++", "language"],
    },
    IconEntry {
        name: "dev-c",
        glyph: '\u{e771}',
        group: IconGroup::Technology,
        keywords: &["c", "language"],
    },
    IconEntry {
        name: "dev-swift",
        glyph: '\u{e755}',
        group: IconGroup::Technology,
        keywords: &["swift", "ios", "language"],
    },
    IconEntry {
        name: "dev-kotlin",
        glyph: '\u{e81b}',
        group: IconGroup::Technology,
        keywords: &["kotlin", "android", "language"],
    },
    IconEntry {
        name: "dev-scala",
        glyph: '\u{e737}',
        group: IconGroup::Technology,
        keywords: &["scala", "jvm", "language"],
    },
    IconEntry {
        name: "dev-haskell",
        glyph: '\u{e777}',
        group: IconGroup::Technology,
        keywords: &["haskell", "functional", "language"],
    },
    IconEntry {
        name: "dev-elixir",
        glyph: '\u{e7cd}',
        group: IconGroup::Technology,
        keywords: &["elixir", "erlang", "language"],
    },
    IconEntry {
        name: "dev-lua",
        glyph: '\u{e826}',
        group: IconGroup::Technology,
        keywords: &["lua", "scripting", "language"],
    },
    IconEntry {
        name: "dev-dart",
        glyph: '\u{e798}',
        group: IconGroup::Technology,
        keywords: &["dart", "flutter", "language"],
    },
    IconEntry {
        name: "dev-r",
        glyph: '\u{e881}',
        group: IconGroup::Technology,
        keywords: &["r", "statistics", "language"],
    },
    IconEntry {
        name: "dev-perl",
        glyph: '\u{e769}',
        group: IconGroup::Technology,
        keywords: &["perl", "scripting", "language"],
    },
    IconEntry {
        name: "dev-html5",
        glyph: '\u{e736}',
        group: IconGroup::Technology,
        keywords: &["html", "markup", "web"],
    },
    IconEntry {
        name: "dev-css3",
        glyph: '\u{e749}',
        group: IconGroup::Technology,
        keywords: &["css", "style", "web"],
    },
    IconEntry {
        name: "dev-sass",
        glyph: '\u{e74b}',
        group: IconGroup::Technology,
        keywords: &["sass", "scss", "css"],
    },
    IconEntry {
        name: "dev-markdown",
        glyph: '\u{e73e}',
        group: IconGroup::Technology,
        keywords: &["markdown", "docs", "text"],
    },
    IconEntry {
        name: "dev-json",
        glyph: '\u{e80b}',
        group: IconGroup::Technology,
        keywords: &["json", "data", "format"],
    },
    IconEntry {
        name: "dev-yaml",
        glyph: '\u{e8eb}',
        group: IconGroup::Technology,
        keywords: &["yaml", "config", "format"],
    },
    IconEntry {
        name: "dev-graphql",
        glyph: '\u{e7f4}',
        group: IconGroup::Technology,
        keywords: &["graphql", "api", "query"],
    },
    IconEntry {
        name: "dev-react",
        glyph: '\u{e7ba}',
        group: IconGroup::Technology,
        keywords: &["react", "frontend", "framework"],
    },
    IconEntry {
        name: "dev-vuejs",
        glyph: '\u{e8dc}',
        group: IconGroup::Technology,
        keywords: &["vue", "frontend", "framework"],
    },
    IconEntry {
        name: "dev-angular",
        glyph: '\u{e753}',
        group: IconGroup::Technology,
        keywords: &["angular", "frontend", "framework"],
    },
    IconEntry {
        name: "dev-svelte",
        glyph: '\u{e8b7}',
        group: IconGroup::Technology,
        keywords: &["svelte", "frontend", "framework"],
    },
    IconEntry {
        name: "dev-tailwindcss",
        glyph: '\u{e8ba}',
        group: IconGroup::Technology,
        keywords: &["tailwind", "css", "framework"],
    },
    IconEntry {
        name: "dev-bootstrap",
        glyph: '\u{e747}',
        group: IconGroup::Technology,
        keywords: &["bootstrap", "css", "framework"],
    },
    IconEntry {
        name: "dev-npm",
        glyph: '\u{e71e}',
        group: IconGroup::Technology,
        keywords: &["npm", "node", "package manager"],
    },
    IconEntry {
        name: "dev-yarn",
        glyph: '\u{e8ec}',
        group: IconGroup::Technology,
        keywords: &["yarn", "node", "package manager"],
    },
    IconEntry {
        name: "dev-nodejs",
        glyph: '\u{e719}',
        group: IconGroup::Technology,
        keywords: &["node", "nodejs", "javascript"],
    },
    IconEntry {
        name: "dev-django",
        glyph: '\u{e71d}',
        group: IconGroup::Technology,
        keywords: &["django", "python", "backend"],
    },
    IconEntry {
        name: "dev-flask",
        glyph: '\u{e7dc}',
        group: IconGroup::Technology,
        keywords: &["flask", "python", "backend"],
    },
    IconEntry {
        name: "dev-rails",
        glyph: '\u{e73b}',
        group: IconGroup::Technology,
        keywords: &["rails", "ruby", "backend"],
    },
    IconEntry {
        name: "dev-spring",
        glyph: '\u{e8ac}',
        group: IconGroup::Technology,
        keywords: &["spring", "java", "backend"],
    },
    IconEntry {
        name: "dev-dotnetcore",
        glyph: '\u{e7c6}',
        group: IconGroup::Technology,
        keywords: &["dotnet", "csharp", "backend"],
    },
    IconEntry {
        name: "dev-docker",
        glyph: '\u{e7b0}',
        group: IconGroup::Technology,
        keywords: &["docker", "container", "devops"],
    },
    IconEntry {
        name: "dev-kubernetes",
        glyph: '\u{e81d}',
        group: IconGroup::Technology,
        keywords: &["kubernetes", "k8s", "orchestration"],
    },
    IconEntry {
        name: "dev-terraform",
        glyph: '\u{e8bd}',
        group: IconGroup::Technology,
        keywords: &["terraform", "infrastructure", "devops"],
    },
    IconEntry {
        name: "dev-linux",
        glyph: '\u{e712}',
        group: IconGroup::Technology,
        keywords: &["linux", "os", "unix"],
    },
    IconEntry {
        name: "dev-apple",
        glyph: '\u{e711}',
        group: IconGroup::Technology,
        keywords: &["apple", "macos", "ios"],
    },
    IconEntry {
        name: "dev-windows11",
        glyph: '\u{e8e5}',
        group: IconGroup::Technology,
        keywords: &["windows", "microsoft", "os"],
    },
    IconEntry {
        name: "dev-ubuntu",
        glyph: '\u{e73a}',
        group: IconGroup::Technology,
        keywords: &["ubuntu", "linux", "debian"],
    },
    IconEntry {
        name: "dev-debian",
        glyph: '\u{e77d}',
        group: IconGroup::Technology,
        keywords: &["debian", "linux"],
    },
    IconEntry {
        name: "dev-archlinux",
        glyph: '\u{e732}',
        group: IconGroup::Technology,
        keywords: &["arch", "linux"],
    },
    IconEntry {
        name: "dev-nixos",
        glyph: '\u{e843}',
        group: IconGroup::Technology,
        keywords: &["nix", "nixos", "linux"],
    },
    IconEntry {
        name: "dev-fedora",
        glyph: '\u{e7d9}',
        group: IconGroup::Technology,
        keywords: &["fedora", "linux", "redhat"],
    },
    IconEntry {
        name: "dev-android",
        glyph: '\u{e70e}',
        group: IconGroup::Technology,
        keywords: &["android", "mobile", "google"],
    },
    IconEntry {
        name: "dev-postgresql",
        glyph: '\u{e76e}',
        group: IconGroup::Technology,
        keywords: &["postgres", "postgresql", "database", "sql"],
    },
    IconEntry {
        name: "dev-mysql",
        glyph: '\u{e704}',
        group: IconGroup::Technology,
        keywords: &["mysql", "database", "sql"],
    },
    IconEntry {
        name: "dev-sqlite",
        glyph: '\u{e7c4}',
        group: IconGroup::Technology,
        keywords: &["sqlite", "database", "sql"],
    },
    IconEntry {
        name: "dev-mongodb",
        glyph: '\u{e7a4}',
        group: IconGroup::Technology,
        keywords: &["mongodb", "database", "nosql"],
    },
    IconEntry {
        name: "dev-redis",
        glyph: '\u{e76d}',
        group: IconGroup::Technology,
        keywords: &["redis", "database", "cache"],
    },
    IconEntry {
        name: "dev-amazonwebservices",
        glyph: '\u{e7ad}',
        group: IconGroup::Technology,
        keywords: &["aws", "amazon", "cloud"],
    },
    IconEntry {
        name: "dev-azure",
        glyph: '\u{e754}',
        group: IconGroup::Technology,
        keywords: &["azure", "microsoft", "cloud"],
    },
    IconEntry {
        name: "dev-googlecloud",
        glyph: '\u{e7f1}',
        group: IconGroup::Technology,
        keywords: &["gcp", "google", "cloud"],
    },
    IconEntry {
        name: "dev-heroku",
        glyph: '\u{e77b}',
        group: IconGroup::Technology,
        keywords: &["heroku", "cloud", "hosting"],
    },
    IconEntry {
        name: "dev-netlify",
        glyph: '\u{e83c}',
        group: IconGroup::Technology,
        keywords: &["netlify", "hosting", "deploy"],
    },
    IconEntry {
        name: "dev-vercel",
        glyph: '\u{e8d3}',
        group: IconGroup::Technology,
        keywords: &["vercel", "hosting", "deploy"],
    },
    IconEntry {
        name: "dev-digital_ocean",
        glyph: '\u{e7ae}',
        group: IconGroup::Technology,
        keywords: &["digitalocean", "cloud", "hosting"],
    },
    IconEntry {
        name: "dev-cloudflare",
        glyph: '\u{e792}',
        group: IconGroup::Technology,
        keywords: &["cloudflare", "cdn", "dns"],
    },
    IconEntry {
        name: "dev-firebase",
        glyph: '\u{e787}',
        group: IconGroup::Technology,
        keywords: &["firebase", "google", "backend"],
    },
    IconEntry {
        name: "dev-git",
        glyph: '\u{e702}',
        group: IconGroup::Technology,
        keywords: &["git", "vcs", "version control"],
    },
    IconEntry {
        name: "dev-github",
        glyph: '\u{e709}',
        group: IconGroup::Technology,
        keywords: &["github", "git", "hosting"],
    },
    IconEntry {
        name: "dev-gitlab",
        glyph: '\u{e7eb}',
        group: IconGroup::Technology,
        keywords: &["gitlab", "git", "hosting"],
    },
    IconEntry {
        name: "dev-bitbucket",
        glyph: '\u{e703}',
        group: IconGroup::Technology,
        keywords: &["bitbucket", "git", "hosting"],
    },
    IconEntry {
        name: "dev-vim",
        glyph: '\u{e7c5}',
        group: IconGroup::Technology,
        keywords: &["vim", "editor"],
    },
    IconEntry {
        name: "dev-vscode",
        glyph: '\u{e8da}',
        group: IconGroup::Technology,
        keywords: &["vscode", "editor"],
    },
    IconEntry {
        name: "dev-jenkins",
        glyph: '\u{e767}',
        group: IconGroup::Technology,
        keywords: &["jenkins", "ci", "devops"],
    },
    IconEntry {
        name: "dev-githubactions",
        glyph: '\u{e7e9}',
        group: IconGroup::Technology,
        keywords: &["github actions", "ci", "devops"],
    },
    IconEntry {
        name: "seti-makefile",
        glyph: '\u{e673}',
        group: IconGroup::Technology,
        keywords: &["makefile", "build", "make"],
    },
];

/// The Catalog names this Catalog carries, in [`entries`] order. What an
/// Errand's reply schema enumerates a Session's or Workspace's Icon over, so
/// a reply is valid by construction: a Model can only choose a name the
/// Catalog actually resolves.
static NAMES: LazyLock<Vec<&'static str>> =
    LazyLock::new(|| CATALOG.iter().map(|entry| entry.name).collect());

/// The glyph a Catalog `name` resolves to, or `None` for a name the Catalog
/// does not — or no longer — carry. What a client draws an Icon with; an
/// unresolved name draws as no Icon at all, which is the point of storing a
/// name rather than a codepoint.
pub(crate) fn glyph(name: &str) -> Option<char> {
    entries()
        .iter()
        .find(|entry| entry.name == name)
        .map(|entry| entry.glyph)
}

/// The Icon Catalog's whole listing, in the order the Icon Picker's grid
/// presents it.
pub(crate) fn entries() -> &'static [IconEntry] {
    CATALOG
}

/// The Catalog names this Catalog carries, in listing order. What an Errand's
/// reply schema enumerates an Icon over.
pub(crate) fn names() -> &'static [&'static str] {
    &NAMES
}

/// The entries whose name or keywords contain `query`, case-insensitively, in
/// listing order — what the Icon Picker narrows its grid to as the user
/// types. An empty or all-whitespace `query` matches everything, which is
/// what the Picker opens showing.
pub(crate) fn search(query: &str) -> Vec<&'static IconEntry> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return entries().iter().collect();
    }
    entries()
        .iter()
        .filter(|entry| {
            entry.name.contains(&query)
                || entry
                    .keywords
                    .iter()
                    .any(|keyword| keyword.contains(&query))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, path::Path};

    use serde_json::Value;

    use super::*;

    /// Nerd Fonts' published glyph names, checked in under `tests/fixtures`
    /// so this pins against a copy Suru controls rather than the network.
    /// Located through `CARGO_MANIFEST_DIR` rather than a relative literal so
    /// the path is correct regardless of the working directory a test runner
    /// starts in, and built with `Path::join` rather than a literal separator
    /// so it resolves on Windows too.
    fn nerd_fonts_glyph_names() -> Value {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("glyphnames.json");
        let raw = std::fs::read_to_string(&fixture)
            .unwrap_or_else(|error| panic!("read {}: {error}", fixture.display()));
        serde_json::from_str(&raw).expect("glyphnames.json fixture is valid JSON")
    }

    /// The Catalog exists to be trusted by name: every entry's codepoint must
    /// match what Nerd Fonts itself publishes for that name, or a client
    /// drawing the glyph a name is supposed to mean draws the wrong one.
    #[test]
    fn every_codepoint_is_pinned_to_the_nerd_fonts_fixture() {
        let published = nerd_fonts_glyph_names();
        for entry in entries() {
            let record = published
                .get(entry.name)
                .unwrap_or_else(|| panic!("`{}` is not a Nerd Font glyph name", entry.name));
            let code = record["code"]
                .as_str()
                .unwrap_or_else(|| panic!("`{}` has no `code` in the fixture", entry.name));
            let published_codepoint = u32::from_str_radix(code, 16)
                .unwrap_or_else(|_| panic!("`{}` has a non-hex `code`: {code}", entry.name));
            let published_glyph = char::from_u32(published_codepoint)
                .unwrap_or_else(|| panic!("`{}` publishes an invalid codepoint", entry.name));
            assert_eq!(
                entry.glyph, published_glyph,
                "`{}` is pinned to the wrong codepoint",
                entry.name
            );
        }
    }

    #[test]
    fn every_name_is_unique() {
        let mut seen = HashSet::new();
        for entry in entries() {
            assert!(
                seen.insert(entry.name),
                "`{}` appears more than once in the Icon Catalog",
                entry.name
            );
        }
    }

    #[test]
    fn every_group_contributes_entries() {
        assert!(
            entries()
                .iter()
                .any(|entry| entry.group == IconGroup::Subject),
            "no entry belongs to the Subjects group"
        );
        assert!(
            entries()
                .iter()
                .any(|entry| entry.group == IconGroup::Technology),
            "no entry belongs to the Technologies group"
        );
    }

    #[test]
    fn every_entry_has_lowercase_keywords() {
        for entry in entries() {
            assert!(
                !entry.keywords.is_empty(),
                "`{}` has no search keywords",
                entry.name
            );
            for keyword in entry.keywords {
                assert_eq!(
                    *keyword,
                    keyword.to_lowercase(),
                    "`{}` has an uppercase keyword: `{keyword}`",
                    entry.name
                );
            }
        }
    }

    #[test]
    fn names_matches_the_listing_in_order() {
        assert_eq!(
            names(),
            entries()
                .iter()
                .map(|entry| entry.name)
                .collect::<Vec<_>>()
                .as_slice()
        );
    }

    #[test]
    fn the_catalog_size_is_in_the_expected_range() {
        let count = entries().len();
        assert!(
            (150..=250).contains(&count),
            "the Icon Catalog has {count} entries, expected roughly 150 to 250"
        );
    }

    #[test]
    fn lookup_by_name_resolves_a_known_entry_and_refuses_an_unknown_one() {
        assert_eq!(glyph("md-bug"), Some('\u{f00e4}'));
        assert_eq!(glyph("not-a-real-icon"), None);
    }

    #[test]
    fn search_is_case_insensitive_and_matches_name_or_keywords() {
        let by_name = search("MD-BUG");
        assert!(by_name.iter().any(|entry| entry.name == "md-bug"));

        let by_keyword = search("Rust");
        assert!(by_keyword.iter().any(|entry| entry.name == "dev-rust"));
    }

    #[test]
    fn an_empty_query_returns_the_whole_listing() {
        assert_eq!(search("").len(), entries().len());
        assert_eq!(search("   ").len(), entries().len());
    }
}
