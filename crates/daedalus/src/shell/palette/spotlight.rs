use std::ops::Range;

use super::{Entry, Mode, rank_entries};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Group {
    Files,
    Symbols,
    Commands,
}

impl Group {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::Files => "Files",
            Self::Symbols => "Symbols",
            Self::Commands => "Commands",
        }
    }

    fn cap(self) -> usize {
        match self {
            Self::Files | Self::Symbols => 5,
            Self::Commands => 3,
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    pub(super) files: Vec<Entry>,
    pub(super) symbols: Vec<Entry>,
    pub(super) commands: Vec<Entry>,
    pub(super) sections: Vec<(Group, Range<usize>)>,
    pub(super) scroll: gpui::ScrollHandle,
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
    ] {
        let start = hits.len();
        for (_, i, positions) in rank_entries(pool, query, Mode::Everything, false)
            .into_iter()
            .take(group.cap())
        {
            hits.push((entries.len(), positions));
            entries.push(pool[i].clone());
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
        State {
            files: rows("item", files),
            symbols: rows("item", symbols),
            commands: rows("item", commands),
            sections: Vec::new(),
            scroll: Default::default(),
        }
    }

    #[test]
    fn arrange_lists_groups_in_order_and_caps_each() {
        let (entries, hits, sections) = arrange(&state(10, 10, 10), "item");
        assert_eq!(entries.len(), 13);
        assert_eq!(hits.len(), 13);
        assert_eq!(
            sections,
            vec![
                (Group::Files, 0..5),
                (Group::Symbols, 5..10),
                (Group::Commands, 10..13)
            ]
        );
    }

    #[test]
    fn arrange_skips_empty_groups() {
        let (_, _, sections) = arrange(&state(2, 0, 1), "item");
        assert_eq!(
            sections,
            vec![(Group::Files, 0..2), (Group::Commands, 2..3)]
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
        assert_eq!(sections.len(), 3);
    }

    #[test]
    fn jump_wraps_between_section_starts() {
        let sections = vec![(Group::Files, 0..5), (Group::Symbols, 5..8)];
        assert_eq!(jump(&sections, 2, 1), 5);
        assert_eq!(jump(&sections, 6, 1), 0);
        assert_eq!(jump(&sections, 0, -1), 5);
        assert_eq!(jump(&[], 0, 1), 0);
    }
}
