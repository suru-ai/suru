//! The matching a picker narrows its rows by.
//!
//! It is not the only matching Suru does: the Command completion ranks its
//! candidates with `commands::fuzzy_score`, and the Sidebar asks the stricter
//! question of whether a Title plainly carries the query. This is the loose
//! one the pickers share.

/// Whether `candidate` carries `query`: its characters in order, but not
/// necessarily together, and in whatever case the reader reached for. An empty
/// query is carried by everything, so a reader who has typed nothing is shown
/// the whole list.
pub(super) fn fuzzy_matches(query: &str, candidate: &str) -> bool {
    let mut candidate = candidate.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|character| candidate.by_ref().any(|candidate| candidate == character))
}
