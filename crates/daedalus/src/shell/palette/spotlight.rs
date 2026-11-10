use std::ops::Range;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use daedalus_lsp::Position;
use daedalus_ui::motion::Closing;
use daedalus_ui::{ActiveTheme, Theme, icon_for};
use gpui::{
    AnyElement, Bounds, ClickEvent, Context, FontWeight, HighlightStyle, MouseButton, SharedString,
    StyledText, Task, Window, canvas, div, fill, point, prelude::*, px, size, svg,
};

use super::{Entry, MAX_ROWS, Mode, Palette, Target, label_highlights, rank_entries};
use crate::actions;
use crate::shell::Shell;
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

    fn chip_id(self) -> &'static str {
        match self {
            Self::Files => "chip-files",
            Self::Symbols => "chip-symbols",
            Self::Commands => "chip-commands",
            Self::Text => "chip-text",
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
    pub(super) scope: Option<Group>,
    /// The groups with results for the query, whatever the scope, so the chips stay put while one is picked.
    pub(super) chips: Vec<Group>,
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
/// group's rows sit in the hits. With a `scope` only that group is listed, up to `MAX_ROWS`.
pub(super) fn arrange(
    state: &State,
    query: &str,
    scope: Option<Group>,
) -> (Vec<Entry>, Hits, Sections) {
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
        if scope.is_some_and(|only| only != group) {
            continue;
        }
        let cap = if scope.is_some() {
            MAX_ROWS
        } else {
            group.cap()
        };
        let start = hits.len();
        for (_, i, positions) in rank_entries(pool, query, Mode::Everything, false)
            .into_iter()
            .take(cap)
        {
            hits.push((entries.len(), positions));
            entries.push(pool[i].clone());
        }
        if group == Group::Text {
            let text_cap = if scope.is_some() { usize::MAX } else { cap };
            for (entry, positions) in state.text.iter().take(text_cap) {
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

/// The groups to offer as chips: those with rows plus the scoped one, in group order.
pub(super) fn chips(sections: &[(Group, Range<usize>)], scope: Option<Group>) -> Vec<Group> {
    [Group::Files, Group::Symbols, Group::Commands, Group::Text]
        .into_iter()
        .filter(|group| Some(*group) == scope || sections.iter().any(|(g, _)| g == group))
        .collect()
}

/// What Enter does on a row, shown beside the selected one.
pub(super) fn row_hint(target: &Target) -> &'static str {
    match target {
        Target::Symbol(..) => "Go to",
        Target::Command(_) | Target::Claude(_) => "Run",
        Target::ProjectSearch(_) => "Search",
        _ => "Open",
    }
}

/// The row to select after the list is rebuilt: the search-in-project row stays last, others keep their index.
pub(super) fn keep_selection(selected: usize, on_search_row: bool, rows: usize) -> usize {
    if on_search_row {
        rows.saturating_sub(1)
    } else {
        selected.min(rows.saturating_sub(1))
    }
}

/// Top and height in px of the overlay scrollbar thumb inside a track, or `None` when the rows fit.
pub(super) fn scroll_thumb(
    view: f32,
    max_offset: f32,
    offset: f32,
    track: f32,
    min_h: f32,
) -> Option<(f32, f32)> {
    if view <= 0. || max_offset <= 0.5 || track <= 0. {
        return None;
    }
    let height = (track * view / (view + max_offset)).max(min_h).min(track);
    let top = (offset / max_offset).clamp(0., 1.) * (track - height);
    Some((top, height))
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

impl Shell {
    /// Picks a chip: the group to list alone, or `None` for every group.
    fn set_spotlight_scope(&mut self, scope: Option<Group>, cx: &mut Context<Self>) {
        if let Some(spotlight) = self.palette.as_mut().and_then(|p| p.spotlight.as_mut()) {
            spotlight.scope = scope;
        }
        self.filter_palette(cx);
    }

    pub(super) fn render_spotlight(
        &self,
        palette: &Palette,
        closing: Option<Closing>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = cx.theme().clone();
        let Some(state) = palette.spotlight.as_ref() else {
            return div().into_any_element();
        };
        let viewport = window.viewport_size();
        let query_empty = palette.input.read(cx).text().is_empty();
        let scoped = state.scope.is_some();

        let rows: Vec<AnyElement> = palette
            .hits
            .iter()
            .enumerate()
            .map(|(row, (index, positions))| {
                let entry = &palette.entries[*index];
                let group = state
                    .sections
                    .iter()
                    .find(|(_, rows)| rows.contains(&row))
                    .map(|(group, _)| *group);
                let header = (!scoped)
                    .then(|| state.sections.iter().find(|(_, rows)| rows.start == row))
                    .flatten()
                    .map(|(group, _)| group.title());
                let row_el = self.spotlight_row(palette, row, entry, positions, group, &t, cx);
                match header {
                    Some(title) => div()
                        .child(
                            div()
                                .h(t.ui(28.))
                                .pl(t.ui(8. + 10. + 32. + 12.))
                                .flex()
                                .items_center()
                                .text_size(t.typography.caption)
                                .text_color(t.color.content_muted)
                                .child(title),
                        )
                        .child(row_el)
                        .into_any_element(),
                    None => row_el,
                }
            })
            .collect();

        let chip = |id: &'static str, label: &'static str, group: Option<Group>| {
            let active = state.scope == group;
            div()
                .id(id)
                .h(t.ui(28.))
                .px(t.ui(12.))
                .flex()
                .flex_none()
                .items_center()
                .rounded_full()
                .cursor_pointer()
                .text_size(t.typography.body)
                .when(active, |el| {
                    el.bg(t.color.accent).text_color(t.color.content_on_accent)
                })
                .when(!active, |el| {
                    el.bg(t.color.surface_hover)
                        .text_color(t.color.content)
                        .hover(|s| s.bg(t.color.surface_active))
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.set_spotlight_scope(group, cx)
                }))
                .child(label)
        };
        let chips = div()
            .h(t.ui(40.))
            .flex_none()
            .px(t.ui(16.))
            .flex()
            .items_center()
            .gap(t.ui(8.))
            .overflow_hidden()
            .child(chip("chip-all", "All", None))
            .children(
                state
                    .chips
                    .iter()
                    .map(|group| chip(group.chip_id(), group.title(), Some(*group))),
            );

        let scroll = state.scroll.clone();
        let (thumb_color, thumb_w, thumb_gap) = (t.color.border_strong, t.ui(4.), t.ui(4.));
        let (inset_top, inset_bottom, min_h) = (t.ui(4.), t.ui(12.), f32::from(t.ui(24.)));
        let thumb = canvas(
            move |bounds, _, _| {
                let track = f32::from(bounds.size.height - inset_top - inset_bottom);
                let (top, height) = scroll_thumb(
                    f32::from(scroll.bounds().size.height),
                    f32::from(scroll.max_offset().height),
                    -f32::from(scroll.offset().y),
                    track,
                    min_h,
                )?;
                Some(Bounds::new(
                    point(
                        bounds.right() - thumb_w - thumb_gap,
                        bounds.top() + inset_top + px(top),
                    ),
                    size(thumb_w, px(height)),
                ))
            },
            move |_, thumb, window, _| {
                if let Some(thumb) = thumb {
                    window.paint_quad(fill(thumb, thumb_color).corner_radii(thumb_w / 2.));
                }
            },
        )
        .absolute()
        .inset_0();
        let nothing = !query_empty && palette.hits.is_empty();
        let panel =
            div()
                .id("palette")
                .key_context("Spotlight")
                .on_action(cx.listener(|this, _: &actions::NextGroup, _, cx| {
                    this.jump_spotlight_group(1, cx)
                }))
                .on_action(cx.listener(|this, _: &actions::PrevGroup, _, cx| {
                    this.jump_spotlight_group(-1, cx)
                }))
                .on_action(
                    cx.listener(|this, _: &actions::OpenInSearchPanel, window, cx| {
                        this.open_in_search_panel(window, cx)
                    }),
                )
                .w(t.ui(720.).min(viewport.width - px(48.)))
                .max_h(viewport.height * 0.6)
                .flex()
                .flex_col()
                .bg(t.color.surface)
                .border_1()
                .border_color(t.color.border)
                .rounded(t.ui(20.))
                .shadow(vec![t.popover_shadow()])
                .overflow_hidden()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .when(closing.is_some(), |el| {
                    el.capture_any_mouse_down(|_, _, cx| cx.stop_propagation())
                })
                .child(
                    div()
                        .h(t.ui(60.))
                        .flex_none()
                        .px(t.ui(18.))
                        .flex()
                        .items_center()
                        .gap(t.ui(12.))
                        .child(
                            svg()
                                .path("ui/search.svg")
                                .size(t.ui(22.))
                                .flex_none()
                                .text_color(t.color.content_muted),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(t.ui(24.))
                                .child(palette.input.clone()),
                        ),
                )
                .when(!query_empty, |el| {
                    el.child(
                        div()
                            .mx(t.ui(16.))
                            .flex_none()
                            .border_b_1()
                            .border_color(t.color.border),
                    )
                    .child(chips)
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .id("palette-rows")
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&state.scroll)
                                    .pb(t.ui(8.))
                                    .children(rows)
                                    .children(nothing.then(|| {
                                        div()
                                            .h(t.ui(52.))
                                            .px(t.ui(18.))
                                            .flex()
                                            .items_center()
                                            .text_color(t.color.content_muted)
                                            .child("Nothing matches")
                                    })),
                            )
                            .child(thumb),
                    )
                });
        self.palette_layer(panel, closing, viewport.height * 0.18, &t, cx)
    }

    #[allow(clippy::too_many_arguments)]
    fn spotlight_row(
        &self,
        palette: &Palette,
        row: usize,
        entry: &Entry,
        positions: &[usize],
        group: Option<Group>,
        t: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = row == palette.selected;
        let bold: Vec<_> = label_highlights(&entry.key, &entry.label, positions)
            .into_iter()
            .map(|range| {
                (
                    range,
                    HighlightStyle {
                        font_weight: Some(FontWeight::SEMIBOLD),
                        ..Default::default()
                    },
                )
            })
            .collect();
        let tile = div()
            .size(t.ui(32.))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(t.ui(8.))
            .bg(t.color.surface_hover)
            .child(match &entry.target {
                Target::File(path) => {
                    let icon = icon_for(path, false, t);
                    svg()
                        .path(icon.asset)
                        .size(t.ui(22.))
                        .flex_none()
                        .text_color(icon.color)
                }
                target => svg()
                    .path(match (group, target) {
                        (_, Target::ProjectSearch(_)) => "ui/search.svg",
                        (Some(Group::Symbols), _) => "ui/symbol.svg",
                        (Some(Group::Commands), _) => "ui/command.svg",
                        _ => "ui/text.svg",
                    })
                    .size(t.ui(18.))
                    .flex_none()
                    .text_color(t.color.content_muted),
            });
        let subtitle = matches!(entry.target, Target::File(_) | Target::Symbol(..))
            .then(|| entry.detail.clone())
            .flatten();
        let side = matches!(entry.target, Target::Command(_))
            .then(|| entry.detail.clone())
            .flatten()
            .or_else(|| entry.kind.map(str::to_string));
        div()
            .id(("palette-row", row))
            .h(t.ui(52.))
            .mx(t.ui(8.))
            .px(t.ui(10.))
            .flex()
            .items_center()
            .gap(t.ui(12.))
            .rounded(t.ui(12.))
            .cursor_pointer()
            .text_color(t.color.content)
            .when(selected, |el| el.bg(t.color.surface_active))
            .when(!selected, |el| el.hover(|s| s.bg(t.color.surface_hover)))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.run_palette(Some(row), event.modifiers().platform, window, cx)
            }))
            .child(tile)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .justify_center()
                    .child(
                        div()
                            .text_size(t.ui(15.))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(
                                StyledText::new(SharedString::from(entry.label.clone()))
                                    .with_highlights(bold),
                            ),
                    )
                    .children(subtitle.map(|d| {
                        div()
                            .text_size(t.typography.caption)
                            .text_color(t.color.content_muted)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(d)
                    })),
            )
            .children(side.map(|k| {
                div()
                    .flex_none()
                    .font_family(t.typography.mono.clone())
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child(k)
            }))
            .children(selected.then(|| {
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(t.ui(8.))
                    .text_size(t.typography.caption)
                    .text_color(t.color.content_muted)
                    .child(row_hint(&entry.target))
                    .child(
                        div()
                            .px(t.ui(6.))
                            .rounded(t.ui(5.))
                            .bg(t.color.surface_hover)
                            .font_family(t.typography.mono.clone())
                            .child("↵"),
                    )
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::shell::palette::Target;

    #[test]
    fn scroll_thumb_hides_when_rows_fit() {
        assert_eq!(scroll_thumb(300., 0., 0., 300., 24.), None);
        assert_eq!(scroll_thumb(0., 300., 0., 0., 24.), None);
    }

    #[test]
    fn scroll_thumb_spans_proportionally_at_top_and_bottom() {
        assert_eq!(scroll_thumb(300., 300., 0., 300., 24.), Some((0., 150.)));
        assert_eq!(
            scroll_thumb(300., 300., 300., 300., 24.),
            Some((150., 150.))
        );
    }

    #[test]
    fn scroll_thumb_keeps_min_height_on_long_lists() {
        assert_eq!(scroll_thumb(300., 30000., 0., 300., 24.), Some((0., 24.)));
        assert_eq!(
            scroll_thumb(300., 30000., 30000., 300., 24.),
            Some((276., 24.))
        );
    }

    #[test]
    fn scroll_thumb_stays_inside_a_shorter_track() {
        assert_eq!(
            scroll_thumb(300., 300., 300., 284., 24.),
            Some((142., 142.))
        );
    }

    #[test]
    fn scroll_thumb_clamps_overscroll() {
        assert_eq!(scroll_thumb(300., 300., -50., 300., 24.), Some((0., 150.)));
        assert_eq!(
            scroll_thumb(300., 300., 400., 300., 24.),
            Some((150., 150.))
        );
    }

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
        let (entries, hits, sections) = arrange(&state(10, 10, 10), "item", None);
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
        let (_, _, sections) = arrange(&state(2, 0, 1), "item", None);
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
        let (entries, hits, sections) = arrange(&state(3, 3, 3), "", None);
        assert!(entries.is_empty() && hits.is_empty() && sections.is_empty());
    }

    #[test]
    fn group_ranges_point_at_entries_of_that_group() {
        let mut s = state(0, 0, 0);
        s.files = rows("file", 2);
        s.symbols = rows("sym", 3);
        s.commands = rows("cmd", 1);
        let (entries, hits, sections) = arrange(&s, "0", None);
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
        let (entries, hits, sections) = arrange(&state(1, 0, 0), "item", None);
        let last = entries[hits.last().unwrap().0].clone();
        assert!(matches!(last.target, Target::ProjectSearch(q) if q == "item"));
        assert_eq!(sections.last(), Some(&(Group::Text, 1..2)));
    }

    #[test]
    fn keep_selection_follows_the_search_row_and_clamps_the_rest() {
        assert_eq!(keep_selection(1, true, 6), 5);
        assert_eq!(keep_selection(1, false, 6), 1);
        assert_eq!(keep_selection(9, false, 6), 5);
        assert_eq!(keep_selection(0, false, 0), 0);
    }

    #[test]
    fn arrange_keeps_text_hits_in_found_order() {
        let mut s = state(0, 0, 0);
        s.text = text_hits(10);
        let (entries, hits, sections) = arrange(&s, "line", None);
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

    #[test]
    fn scoped_arrange_lists_only_that_group_up_to_max_rows() {
        let (entries, hits, sections) = arrange(&state(60, 10, 10), "item", Some(Group::Files));
        assert_eq!(entries.len(), 50);
        assert_eq!(hits.len(), 50);
        assert_eq!(sections, vec![(Group::Files, 0..50)]);
    }

    #[test]
    fn scoped_text_keeps_the_search_in_project_row_last() {
        let mut s = state(3, 3, 3);
        s.text = text_hits(20);
        let (entries, hits, sections) = arrange(&s, "line", Some(Group::Text));
        assert_eq!(sections, vec![(Group::Text, 0..21)]);
        let last = &entries[hits.last().unwrap().0];
        assert!(matches!(&last.target, Target::ProjectSearch(q) if q == "line"));
        assert_eq!(entries[hits[19].0].label, "line19");
    }

    #[test]
    fn unscoped_arrange_is_unchanged() {
        let mut s = state(7, 2, 4);
        s.text = text_hits(12);
        let (entries, hits, sections) = arrange(&s, "item", None);
        assert_eq!(entries.len(), 5 + 2 + 3 + 8 + 1);
        assert_eq!(hits.len(), entries.len());
        assert_eq!(
            sections,
            vec![
                (Group::Files, 0..5),
                (Group::Symbols, 5..7),
                (Group::Commands, 7..10),
                (Group::Text, 10..19)
            ]
        );
    }

    #[test]
    fn chips_follow_groups_with_results_in_order() {
        let (_, _, sections) = arrange(&state(2, 0, 1), "item", None);
        assert_eq!(
            chips(&sections, None),
            vec![Group::Files, Group::Commands, Group::Text]
        );
        assert!(chips(&[], None).is_empty());
    }

    #[test]
    fn row_hint_names_the_action_per_target() {
        let at = Position {
            line: 0,
            character: 0,
        };
        assert_eq!(row_hint(&Target::File(PathBuf::from("a"))), "Open");
        assert_eq!(row_hint(&Target::Symbol(PathBuf::from("a"), at)), "Go to");
        assert_eq!(row_hint(&Target::Claude("x".into())), "Run");
        assert_eq!(row_hint(&Target::ProjectSearch("q".into())), "Search");
    }

    #[test]
    fn scoped_chip_stays_when_its_group_empties() {
        let (_, _, sections) = arrange(&state(0, 0, 1), "item", None);
        assert_eq!(
            chips(&sections, Some(Group::Symbols)),
            vec![Group::Symbols, Group::Commands, Group::Text]
        );
    }
}
