use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use daedalus_lsp::Position;
use gpui::Task;

use super::{Entry, Mode, Target, rank_entries};
use crate::shell::search::Hit;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Group {
    Files,
    Symbols,
    Commands,
    Text,
}

impl Group {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Symbols => "Symbols",
            Self::Commands => "Commands",
            Self::Text => "Text",
        }
    }

    fn cap(self) -> usize {
        match self {
            Self::Files | Self::Symbols => 5,
            Self::Commands => 3,
            Self::Text => 8,
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    pub(super) files: Vec<Entry>,
    pub(super) symbols: Vec<Entry>,
    pub(super) commands: Vec<Entry>,
    /// Matching lines in the order the search found them, with the matched char positions of each label.
    pub(super) text: Vec<(Entry, Vec<usize>)>,
    pub(super) text_cancel: Arc<AtomicBool>,
    pub(super) text_task: Option<Task<()>>,
    pub(super) sections: Vec<(Group, Range<usize>)>,
    pub(super) scroll: gpui::ScrollHandle,
}

impl Drop for State {
    fn drop(&mut self) {
        self.text_cancel.store(true, Ordering::Relaxed);
    }
}

/// The row for a matching line, labelled with the line without its indent.
pub(super) fn text_entry(root: &Path, path: &Path, hit: &Hit) -> (Entry, Vec<usize>) {
    let label = hit.text.trim_start();
    let indent = hit.text.len() - label.len();
    let mut positions = Vec::new();
    for range in &hit.ranges {
        let (start, end) = (range.start.max(indent), range.end);
        if start < end {
            let first = hit.text[indent..start].chars().count();
            positions.extend(first..first + hit.text[start..end].chars().count());
        }
    }
    let rel = path.strip_prefix(root).unwrap_or(path).display();
    let entry = Entry {
        label: label.to_string(),
        detail: Some(format!("{rel}:{}", hit.line + 1)),
        kind: None,
        key: label.to_string(),
        target: Target::Symbol(
            path.to_path_buf(),
            Position {
                line: hit.line,
                character: hit.column,
            },
        ),
    };
    (entry, positions)
}

type Hits = Vec<(usize, Vec<usize>)>;
type Sections = Vec<(Group, Range<usize>)>;

/// The rows shown for `query`: the picked entries, their hits into those entries, and where each
/// group's rows sit in the hits.
pub(super) fn arrange(state: &State, query: &str) -> (Vec<Entry>, Hits, Sections) {
    let mut entries = Vec::new();
    let mut hits = Vec::new();
    let mut sections = Vec::new();
    if query.is_empty() {
        return (entries, hits, sections);
    }
    for (group, pool) in [
        (Group::Files, &state.files),
        (Group::Symbols, &state.symbols),
        (Group::Commands, &state.commands),
        (Group::Text, &Vec::new()),
    ] {
        let start = hits.len();
        for (_, i, positions) in rank_entries(pool, query, Mode::Everything, false)
            .into_iter()
            .take(group.cap())
        {
            hits.push((entries.len(), positions));
            entries.push(pool[i].clone());
        }
        if group == Group::Text {
            for (entry, positions) in state.text.iter().take(group.cap()) {
                hits.push((entries.len(), positions.clone()));
                entries.push(entry.clone());
            }
            let label = format!("Search in project for “{query}”");
            hits.push((entries.len(), Vec::new()));
            entries.push(Entry {
                key: label.clone(),
                label,
                detail: None,
                kind: None,
                target: Target::ProjectSearch(query.to_string()),
            });
        }
        if hits.len() > start {
            sections.push((group, start..hits.len()));
        }
    }
    (entries, hits, sections)
}

/// The first row of the section after (or, with a negative step, before) the one holding
/// `selected`, wrapping around.
pub(super) fn jump(sections: &[(Group, Range<usize>)], selected: usize, step: isize) -> usize {
    let here = sections
        .iter()
        .position(|(_, rows)| rows.contains(&selected))
        .unwrap_or(0);
    let next = (here as isize + step).rem_euclid(sections.len().max(1) as isize) as usize;
    sections.get(next).map_or(0, |(_, rows)| rows.start)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::shell::palette::Target;

    fn rows(prefix: &str, n: usize) -> Vec<Entry> {
        (0..n)
            .map(|i| Entry {
                label: format!("{prefix}{i}"),
                detail: None,
                kind: None,
                key: format!("{prefix}{i}"),
                target: Target::File(PathBuf::from(format!("{prefix}{i}"))),
            })
            .collect()
    }

    fn state(files: usize, symbols: usize, commands: usize) -> State {
        let mut state = State::default();
        state.files = rows("item", files);
        state.symbols = rows("item", symbols);
        state.commands = rows("item", commands);
        state
    }

    fn hit(text: &str, range: Range<usize>) -> Hit {
        Hit {
            line: 6,
            column: 0,
            text: text.into(),
            ranges: vec![range],
        }
    }

    fn text_hits(n: usize) -> Vec<(Entry, Vec<usize>)> {
        (0..n)
            .map(|i| {
                text_entry(
                    Path::new("/p"),
                    Path::new("/p/a.rs"),
                    &hit(&format!("line{i}"), 0..4),
                )
            })
            .collect()
    }

    #[test]
    fn arrange_lists_groups_in_order_and_caps_each() {
        let (entries, hits, sections) = arrange(&state(10, 10, 10), "item");
        assert_eq!(entries.len(), 14);
        assert_eq!(hits.len(), 14);
        assert_eq!(
            sections,
            vec![
                (Group::Files, 0..5),
                (Group::Symbols, 5..10),
                (Group::Commands, 10..13),
                (Group::Text, 13..14)
            ]
        );
    }

    #[test]
    fn arrange_skips_empty_groups() {
        let (_, _, sections) = arrange(&state(2, 0, 1), "item");
        assert_eq!(
            sections,
            vec![
                (Group::Files, 0..2),
                (Group::Commands, 2..3),
                (Group::Text, 3..4)
            ]
        );
    }

    #[test]
    fn arrange_with_empty_query_lists_nothing() {
        let (entries, hits, sections) = arrange(&state(3, 3, 3), "");
        assert!(entries.is_empty() && hits.is_empty() && sections.is_empty());
    }

    #[test]
    fn group_ranges_point_at_entries_of_that_group() {
        let mut s = state(0, 0, 0);
        s.files = rows("file", 2);
        s.symbols = rows("sym", 3);
        s.commands = rows("cmd", 1);
        let (entries, hits, sections) = arrange(&s, "0");
        let labels = |range: Range<usize>| -> Vec<&str> {
            hits[range]
                .iter()
                .map(|(i, _)| entries[*i].label.as_str())
                .collect()
        };
        assert_eq!(labels(0..1), ["file0"]);
        assert_eq!(labels(1..2), ["sym0"]);
        assert_eq!(labels(2..3), ["cmd0"]);
        assert_eq!(sections.len(), 4);
    }

    #[test]
    fn jump_wraps_between_section_starts() {
        let sections = vec![(Group::Files, 0..5), (Group::Symbols, 5..8)];
        assert_eq!(jump(&sections, 2, 1), 5);
        assert_eq!(jump(&sections, 6, 1), 0);
        assert_eq!(jump(&sections, 0, -1), 5);
        assert_eq!(jump(&[], 0, 1), 0);
    }

    #[test]
    fn text_entry_trims_indent_and_keeps_the_match_bold() {
        let (entry, positions) = text_entry(
            Path::new("/p"),
            Path::new("/p/a.rs"),
            &hit("    let é = foo;", 13..16),
        );
        assert_eq!(entry.label, "let é = foo;");
        assert_eq!(positions, vec![8, 9, 10]);
    }

    #[test]
    fn text_entry_detail_is_relative_path_and_one_based_line() {
        let (entry, _) = text_entry(Path::new("/p"), Path::new("/p/src/a.rs"), &hit("x", 0..1));
        assert_eq!(entry.detail.as_deref(), Some("src/a.rs:7"));
        assert!(matches!(entry.target, Target::Symbol(_, at) if at.line == 6));
    }

    #[test]
    fn arrange_ends_with_the_search_in_project_row_even_without_text_hits() {
        let (entries, hits, sections) = arrange(&state(1, 0, 0), "item");
        let last = entries[hits.last().unwrap().0].clone();
        assert!(matches!(last.target, Target::ProjectSearch(q) if q == "item"));
        assert_eq!(sections.last(), Some(&(Group::Text, 1..2)));
    }

    #[test]
    fn arrange_keeps_text_hits_in_found_order() {
        let mut s = state(0, 0, 0);
        s.text = text_hits(10);
        let (entries, hits, sections) = arrange(&s, "line");
        assert_eq!(sections, vec![(Group::Text, 0..9)]);
        let labels: Vec<_> = hits[..8]
            .iter()
            .map(|(i, _)| entries[*i].label.clone())
            .collect();
        assert_eq!(
            labels,
            (0..8).map(|i| format!("line{i}")).collect::<Vec<_>>()
        );
    }
}
