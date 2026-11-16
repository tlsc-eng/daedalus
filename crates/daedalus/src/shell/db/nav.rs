use std::collections::HashSet;

use daedalus_db::Listed;
use daedalus_ui::ActiveTheme;
use gpui::{App, Context, InteractiveElement, ScrollStrategy, Window};

use super::explorer::{Line, Row};
use super::{NodeKey, conn_key};
use crate::actions::{
    CollapseOrParent, ExpandOrChild, Leave, OpenSelected, SelectFirst, SelectLast, SelectNext,
    SelectPageDown, SelectPageUp, SelectPrev,
};
use crate::shell::Shell;
use crate::shell::tree::ROW_HEIGHT;

/// An explorer row named by what it shows, so the selection survives the list being rebuilt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Sel {
    Node(NodeKey),
    Column(NodeKey, String),
    Note(NodeKey, String),
    Detected(String),
    LookInDocker,
}

impl Sel {
    /// The connection key a tree row belongs to; Detected and Look rows have none.
    pub(super) fn conn(&self) -> Option<&str> {
        match self {
            Self::Node(n) | Self::Column(n, _) | Self::Note(n, _) => Some(n.conn()),
            Self::Detected(_) | Self::LookInDocker => None,
        }
    }

    /// The nodes holding this row, nearest first.
    fn ancestors(&self) -> Vec<NodeKey> {
        let mut next = match self {
            Self::Node(n) => parent(n),
            Self::Column(p, _) | Self::Note(p, _) => Some(p.clone()),
            Self::Detected(_) | Self::LookInDocker => None,
        };
        let mut out = Vec::new();
        while let Some(n) = next {
            next = parent(&n);
            out.push(n);
        }
        out
    }
}

fn parent(node: &NodeKey) -> Option<NodeKey> {
    match node {
        NodeKey::Table(c, s, _) => Some(NodeKey::Schema(c.clone(), s.clone())),
        NodeKey::Schema(c, _) => Some(NodeKey::Conn(c.clone())),
        NodeKey::Conn(_) => None,
    }
}

/// What a line selects; None for the lines the keys skip.
pub(super) fn sel_of(line: &Line) -> Option<Sel> {
    match line {
        Line::Tree(row, _) => match row {
            Row::Conn { key, .. } => Some(Sel::Node(NodeKey::Conn(key.clone()))),
            Row::Schema { conn, name, .. } => {
                Some(Sel::Node(NodeKey::Schema(conn.clone(), name.clone())))
            }
            Row::Table {
                conn, schema, name, ..
            } => Some(Sel::Node(NodeKey::Table(
                conn.clone(),
                schema.clone(),
                name.clone(),
            ))),
            Row::Column { parent, name, .. } => Some(Sel::Column(parent.clone(), name.clone())),
            Row::Note {
                parent,
                text,
                retry: Some(_),
                ..
            } => Some(Sel::Note(parent.clone(), text.clone())),
            Row::Note { retry: None, .. } | Row::Error { .. } => None,
        },
        Line::DetectedHeader => None,
        Line::Detected(d) => Some(Sel::Detected(d.key.clone())),
        Line::LookInDocker => Some(Sel::LookInDocker),
    }
}

pub(super) fn find(lines: &[Line], sel: &Sel) -> Option<usize> {
    lines.iter().position(|l| sel_of(l).as_ref() == Some(sel))
}

/// The line standing for `sel`: its own, else its nearest visible ancestor's.
pub(super) fn current(lines: &[Line], sel: &Sel) -> Option<usize> {
    find(lines, sel).or_else(|| {
        sel.ancestors()
            .into_iter()
            .find_map(|a| find(lines, &Sel::Node(a)))
    })
}

/// The selectable line `by` rows from `from`, or the nearest one short of it; `from` at the ends.
fn step(selectable: &[bool], from: Option<usize>, by: isize) -> Option<usize> {
    let len = selectable.len() as isize;
    if !selectable.iter().any(|s| *s) {
        return None;
    }
    let start = match from {
        Some(f) => f as isize,
        None if by > 0 => -1,
        None => len,
    };
    let target = start.saturating_add(by).clamp(0, len - 1);
    (0..len)
        .filter(|&i| selectable[i as usize] && if by > 0 { i > start } else { i < start })
        .min_by_key(|&i| ((i - target).abs(), -(i - start).abs()))
        .map(|i| i as usize)
        .or(from)
}

/// What Left does from a selection.
#[derive(Debug, PartialEq)]
enum Left {
    Collapse(NodeKey),
    Select(Sel),
    Nothing,
}

fn left_move(lines: &[Line], sel: &Sel, expanded: &HashSet<NodeKey>) -> Left {
    if let Sel::Node(n) = sel
        && expanded.contains(n)
    {
        return Left::Collapse(n.clone());
    }
    sel.ancestors()
        .into_iter()
        .map(Sel::Node)
        .find(|a| find(lines, a).is_some())
        .map_or(Left::Nothing, Left::Select)
}

/// Whether a click that collapses `node` leaves the selection alone because it sits inside `node`.
pub(super) fn keeps_selection(selected: Option<&Sel>, node: &NodeKey, expanded: bool) -> bool {
    expanded && selected.is_some_and(|s| s.ancestors().contains(node))
}

/// Rows moved by one page key: a screen minus one row of overlap, never fewer than one.
fn page_rows(list_h: f32, row_h: f32) -> usize {
    if row_h <= 0. || !list_h.is_finite() || !row_h.is_finite() {
        return 1;
    }
    ((list_h / row_h).floor() as usize).saturating_sub(1).max(1)
}

impl Shell {
    /// The lines the keys move over; none while the explorer shows its error or empty state.
    fn db_nav_lines(&self, cx: &App) -> Vec<Line> {
        if self.db.load_error.is_some() {
            return Vec::new();
        }
        let listed = self.db_listed();
        let detected = self.db_detected_rows();
        if self.db.loaded && listed.is_empty() && detected.is_empty() {
            return Vec::new();
        }
        self.db_lines(&listed, detected, cx)
    }

    /// The highlighted line and its selection; pins an ancestor that stands in for a hidden row.
    fn db_effective(&mut self, lines: &[Line]) -> Option<(usize, Sel)> {
        let ix = self.db.selected.as_ref().and_then(|s| current(lines, s))?;
        let sel = sel_of(&lines[ix])?;
        self.db.selected = Some(sel.clone());
        Some((ix, sel))
    }

    fn db_move(&mut self, by: isize, cx: &mut Context<Self>) {
        let lines = self.db_nav_lines(cx);
        let selectable: Vec<bool> = lines.iter().map(|l| sel_of(l).is_some()).collect();
        let from = self.db_effective(&lines).map(|(ix, _)| ix);
        let Some(ix) = step(&selectable, from, by) else {
            return;
        };
        let Some(sel) = sel_of(&lines[ix]) else {
            return;
        };
        let strategy = if by < 0 {
            ScrollStrategy::Top
        } else {
            ScrollStrategy::Bottom
        };
        self.db.select(sel, strategy);
        cx.notify();
    }

    fn db_page(&mut self, dir: isize, cx: &mut Context<Self>) {
        let h = f32::from(self.db.scroll.0.borrow().base_handle.bounds().size.height);
        let row = f32::from(cx.theme().ui(ROW_HEIGHT));
        self.db_move(dir * page_rows(h, row) as isize, cx);
    }

    fn db_collapse_or_parent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lines = self.db_nav_lines(cx);
        let Some((_, sel)) = self.db_effective(&lines) else {
            return;
        };
        match left_move(&lines, &sel, &self.db.expanded) {
            Left::Collapse(n) => self.db_toggle(n, None, window, cx),
            Left::Select(a) => {
                self.db.select(a, ScrollStrategy::Top);
                cx.notify();
            }
            Left::Nothing => {}
        }
    }

    fn db_expand_or_child(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lines = self.db_nav_lines(cx);
        let Some((ix, Sel::Node(n))) = self.db_effective(&lines) else {
            return;
        };
        if !self.db.expanded.contains(&n) {
            let listed = match &n {
                NodeKey::Conn(key) => self.db_listed_conn(key),
                _ => None,
            };
            self.db_toggle(n, listed, window, cx);
            return;
        }
        let child = lines[ix + 1..].iter().find_map(sel_of);
        if let Some(child) = child
            && child.ancestors().contains(&n)
        {
            self.db.select(child, ScrollStrategy::Bottom);
            cx.notify();
        }
    }

    fn db_open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let lines = self.db_nav_lines(cx);
        let Some((_, sel)) = self.db_effective(&lines) else {
            return;
        };
        match sel {
            Sel::Node(NodeKey::Conn(k)) => {
                let listed = self.db_listed_conn(&k);
                self.db_toggle(NodeKey::Conn(k), listed, window, cx);
            }
            Sel::Node(n @ NodeKey::Schema(..)) => self.db_toggle(n, None, window, cx),
            Sel::Node(NodeKey::Table(c, s, t)) | Sel::Column(NodeKey::Table(c, s, t), _) => {
                self.db_open_table(c, s, t, window, cx)
            }
            Sel::Column(..) => {}
            Sel::Note(p, _) => self.db_introspect(p.conn(), cx),
            Sel::Detected(k) => self.db_open_detected(&k, window, cx),
            Sel::LookInDocker => self.containers_start(cx),
        }
    }

    fn db_listed_conn(&self, key: &str) -> Option<Listed> {
        self.db_listed()
            .into_iter()
            .find(|l| conn_key(l.origin, &l.conn.id) == key)
    }
}

/// Binds the explorer's list keys to the shell.
pub(super) fn bind_explorer_keys(el: gpui::Div, cx: &mut Context<Shell>) -> gpui::Div {
    el.on_action(cx.listener(|this, _: &SelectPrev, _, cx| this.db_move(-1, cx)))
        .on_action(cx.listener(|this, _: &SelectNext, _, cx| this.db_move(1, cx)))
        .on_action(cx.listener(|this, _: &SelectFirst, _, cx| this.db_move(isize::MIN, cx)))
        .on_action(cx.listener(|this, _: &SelectLast, _, cx| this.db_move(isize::MAX, cx)))
        .on_action(cx.listener(|this, _: &SelectPageUp, _, cx| this.db_page(-1, cx)))
        .on_action(cx.listener(|this, _: &SelectPageDown, _, cx| this.db_page(1, cx)))
        .on_action(
            cx.listener(|this, _: &CollapseOrParent, w, cx| this.db_collapse_or_parent(w, cx)),
        )
        .on_action(cx.listener(|this, _: &ExpandOrChild, w, cx| this.db_expand_or_child(w, cx)))
        .on_action(cx.listener(|this, _: &OpenSelected, w, cx| this.db_open_selected(w, cx)))
        .on_action(cx.listener(|this, _: &Leave, w, cx| this.focus_active_item(w, cx)))
}

#[cfg(test)]
mod tests {
    use daedalus_db::Engine;

    use super::super::Phase;
    use super::super::detected::DetectedRow;
    use super::super::explorer::Tone;
    use super::*;

    fn conn(key: &str) -> Line {
        Line::Tree(
            Row::Conn {
                key: key.into(),
                name: key.into(),
                engine: Engine::Postgres,
                color: None,
                production: false,
                shared: false,
                read_only: false,
                claude: false,
                claude_busy: false,
                phase: Phase::default(),
                expanded: true,
            },
            1.,
        )
    }

    fn schema(c: &str, s: &str) -> Line {
        Line::Tree(
            Row::Schema {
                conn: c.into(),
                name: s.into(),
                count: 1,
                expanded: true,
            },
            1.,
        )
    }

    fn table(c: &str, s: &str, t: &str) -> Line {
        Line::Tree(
            Row::Table {
                conn: c.into(),
                schema: s.into(),
                name: t.into(),
                kind: daedalus_db::catalog::TableKind::Table,
                expanded: true,
            },
            1.,
        )
    }

    fn table_key(c: &str, s: &str, t: &str) -> NodeKey {
        NodeKey::Table(c.into(), s.into(), t.into())
    }

    fn column(parent: NodeKey, name: &str) -> Line {
        Line::Tree(
            Row::Column {
                parent,
                name: name.into(),
                type_name: "int".into(),
                pk: false,
                nullable: false,
            },
            1.,
        )
    }

    fn note(parent: NodeKey, text: &str, retry: Option<&str>) -> Line {
        Line::Tree(
            Row::Note {
                parent,
                depth: 2,
                text: text.into(),
                tone: Tone::Muted,
                retry: retry.map(String::from),
            },
            1.,
        )
    }

    fn error(key: &str) -> Line {
        Line::Tree(
            Row::Error {
                key: key.into(),
                message: "boom".into(),
            },
            1.,
        )
    }

    fn detected(key: &str) -> Line {
        Line::Detected(DetectedRow {
            key: key.into(),
            name: key.into(),
            engine: Engine::Postgres,
            source: String::new(),
            tip: String::new(),
        })
    }

    #[test]
    fn sel_of_skips_the_detected_header_error_rows_and_plain_notes() {
        let c = NodeKey::Conn("c".into());
        assert_eq!(sel_of(&Line::DetectedHeader), None);
        assert_eq!(sel_of(&error("c")), None);
        assert_eq!(sel_of(&note(c, "Reading tables…", None)), None);
        assert!(sel_of(&conn("c")).is_some());
        assert!(sel_of(&detected("d")).is_some());
        assert_eq!(sel_of(&Line::LookInDocker), Some(Sel::LookInDocker));
        let t = table_key("c", "s", "t");
        assert_eq!(
            sel_of(&column(t.clone(), "id")),
            Some(Sel::Column(t, "id".into()))
        );
    }

    #[test]
    fn a_retry_note_is_selectable_by_parent_and_text() {
        let c = NodeKey::Conn("c".into());
        assert_eq!(
            sel_of(&note(c.clone(), "Could not read tables: x", Some("c"))),
            Some(Sel::Note(c, "Could not read tables: x".into()))
        );
    }

    #[test]
    fn step_moves_one_selectable_line_and_skips_the_rest() {
        let s = [true, false, false, true, true];
        assert_eq!(step(&s, Some(0), 1), Some(3));
        assert_eq!(step(&s, Some(3), 1), Some(4));
        assert_eq!(step(&s, Some(3), -1), Some(0));
    }

    #[test]
    fn step_from_no_selection_starts_at_the_first_or_last_line() {
        let s = [false, true, true, false];
        assert_eq!(step(&s, None, 1), Some(1));
        assert_eq!(step(&s, None, -1), Some(2));
    }

    #[test]
    fn step_stops_at_the_ends() {
        let s = [true, true, false];
        assert_eq!(step(&s, Some(1), 1), Some(1));
        assert_eq!(step(&s, Some(0), -1), Some(0));
    }

    #[test]
    fn a_page_lands_on_the_selectable_line_nearest_its_target() {
        let s = [true, true, false, false, true, true, true];
        assert_eq!(step(&s, Some(0), 2), Some(1));
        assert_eq!(step(&s, Some(0), 3), Some(4));
        assert_eq!(step(&s, Some(0), 50), Some(6));
        assert_eq!(step(&s, Some(6), -50), Some(0));
    }

    #[test]
    fn first_and_last_reach_the_ends_past_unselectable_lines() {
        let s = [false, true, true, false];
        assert_eq!(step(&s, Some(2), isize::MIN), Some(1));
        assert_eq!(step(&s, Some(1), isize::MAX), Some(2));
        assert_eq!(step(&s, None, isize::MIN), Some(1));
        assert_eq!(step(&s, None, isize::MAX), Some(2));
    }

    #[test]
    fn step_finds_nothing_without_selectable_lines() {
        assert_eq!(step(&[false, false], None, 1), None);
        assert_eq!(step(&[], Some(0), -1), None);
    }

    #[test]
    fn ancestors_run_nearest_first() {
        let t = table_key("c", "s", "t");
        let sel = Sel::Column(t.clone(), "id".into());
        assert_eq!(
            sel.ancestors(),
            vec![
                t,
                NodeKey::Schema("c".into(), "s".into()),
                NodeKey::Conn("c".into())
            ]
        );
    }

    #[test]
    fn a_hidden_row_shows_on_its_nearest_visible_ancestor() {
        let sel = Sel::Node(table_key("c", "s", "t"));
        assert_eq!(current(&[conn("c"), schema("c", "s")], &sel), Some(1));
        assert_eq!(current(&[conn("c")], &sel), Some(0));
        assert_eq!(
            current(&[conn("c"), schema("c", "s"), table("c", "s", "t")], &sel),
            Some(2)
        );
    }

    #[test]
    fn a_gone_detected_row_has_no_current_line() {
        let lines = [conn("c"), Line::DetectedHeader, detected("other")];
        assert_eq!(current(&lines, &Sel::Detected("d".into())), None);
        assert_eq!(current(&lines, &Sel::Detected("other".into())), Some(2));
    }

    #[test]
    fn sel_conn_names_the_connection_of_tree_rows_only() {
        let t = table_key("c", "s", "t");
        assert_eq!(Sel::Node(t.clone()).conn(), Some("c"));
        assert_eq!(Sel::Column(t.clone(), "id".into()).conn(), Some("c"));
        assert_eq!(Sel::Note(t, "x".into()).conn(), Some("c"));
        assert_eq!(Sel::Detected("d".into()).conn(), None);
        assert_eq!(Sel::LookInDocker.conn(), None);
    }

    #[test]
    fn left_goes_to_the_parent_or_collapses() {
        let t = table_key("c", "s", "t");
        let s = NodeKey::Schema("c".into(), "s".into());
        let lines = [
            conn("c"),
            schema("c", "s"),
            table("c", "s", "t"),
            column(t.clone(), "id"),
            note(t.clone(), "retry", Some("c")),
        ];
        let open: HashSet<NodeKey> = [t.clone()].into();
        let none = HashSet::new();
        let col = Sel::Column(t.clone(), "id".into());
        assert_eq!(
            left_move(&lines, &col, &open),
            Left::Select(Sel::Node(t.clone()))
        );
        let retry = Sel::Note(t.clone(), "retry".into());
        assert_eq!(
            left_move(&lines, &retry, &open),
            Left::Select(Sel::Node(t.clone()))
        );
        assert_eq!(
            left_move(&lines, &Sel::Node(t.clone()), &open),
            Left::Collapse(t.clone())
        );
        assert_eq!(
            left_move(&lines, &Sel::Node(t), &none),
            Left::Select(Sel::Node(s))
        );
        assert_eq!(
            left_move(&lines, &Sel::Detected("d".into()), &none),
            Left::Nothing
        );
        assert_eq!(left_move(&lines, &Sel::LookInDocker, &none), Left::Nothing);
    }

    #[test]
    fn a_collapsing_click_keeps_a_selection_inside_it() {
        let s = NodeKey::Schema("c".into(), "s".into());
        let inside = Sel::Node(table_key("c", "s", "t"));
        assert!(keeps_selection(Some(&inside), &s, true));
        assert!(!keeps_selection(Some(&inside), &s, false));
        assert!(!keeps_selection(Some(&Sel::Node(s.clone())), &s, true));
        assert!(!keeps_selection(None, &s, true));
        assert!(!keeps_selection(Some(&Sel::Detected("d".into())), &s, true));
    }

    #[test]
    fn page_rows_cases() {
        assert_eq!(page_rows(220., 22.), 9);
        assert_eq!(page_rows(30., 22.), 1);
        assert_eq!(page_rows(0., 22.), 1);
        assert_eq!(page_rows(100., 0.), 1);
        assert_eq!(page_rows(f32::NAN, 22.), 1);
    }
}
