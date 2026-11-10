//! The whole-file blame gutter: who last changed each stretch of lines, and how long ago.

use std::ops::Range;
use std::time::Duration;

use daedalus_ui::ActiveTheme;
use daedalus_ui::motion::{self, Opening};
use gpui::{
    Animation, AnyElement, Bounds, ClickEvent, ClipboardItem, Context, Corner, EventEmitter,
    MouseButton, Pixels, Point, SharedString, Task, anchored, deferred, div, point, prelude::*, px,
};

use crate::view::EditorView;

/// Columns of the gutter the blame text takes.
pub(crate) const BLAME_COLUMNS: usize = 26;

/// What a commit shows in the blame gutter; the shell words it so the editor needs no git.
#[derive(Clone, Debug, PartialEq)]
pub struct BlameCommit {
    pub sha: String,
    pub author: String,
    /// "3 days ago".
    pub age: String,
    pub summary: String,
    /// 1 for the newest commit in the file down to 0 for the oldest, for the age colour.
    pub heat: f32,
}

/// Blame for every line of a file: its commits, and which one each zero-based line came from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GutterBlame {
    pub commits: Vec<BlameCommit>,
    pub lines: Vec<usize>,
}

/// The cursor line's inline blame: its caption, and the commit when the line is committed.
#[derive(Clone, Debug, PartialEq)]
pub struct LineBlame {
    /// Zero-based line.
    pub line: usize,
    pub caption: String,
    pub commit: Option<BlameCommit>,
}

/// What the hover card shows of a commit beyond the caption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub email: String,
    pub date: String,
    pub message: String,
}

/// The pointer rests this long on the caption before the card opens.
const CARD_DELAY: Duration = Duration::from_millis(500);
/// Grace for the pointer to cross from the caption onto the card.
const CARD_HIDE_DELAY: Duration = Duration::from_millis(300);

struct CardShown {
    sha: String,
    info: CommitInfo,
    pull: Option<u64>,
    opened: Opening,
}

/// The inline blame hover card's state on one editor.
#[derive(Default)]
pub(crate) struct BlameCard {
    shown: Option<CardShown>,
    over_caption: bool,
    over_card: bool,
    rest_timer: Option<Task<()>>,
    hide_timer: Option<Task<()>>,
}

/// Something the git gutters ask of whoever knows the repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitGutterEvent {
    /// The blame gutter was clicked on a commit's lines.
    OpenCommit { sha: String },
    /// The inline blame hover card is about to show this commit; answer with
    /// [`EditorView::show_commit_details`], then [`EditorView::show_commit_pr`] if a pull request
    /// is found.
    CommitDetails { sha: String },
    /// "Open pull request" was clicked on the hover card of this commit.
    OpenPullRequest { sha: String },
    /// The quick diff peek is wanted on the change at a zero-based line; answer with
    /// [`EditorView::show_change_peek`].
    PeekChange { line: usize },
    /// Stage one change from the quick diff peek: the index text with it applied, and the
    /// index text it was made from.
    StageChange { contents: String, expected: String },
}

impl EventEmitter<GitGutterEvent> for EditorView {}

/// The blame gutter's state on one editor.
#[derive(Default)]
pub(crate) struct FileBlame {
    pub(crate) blame: Option<GutterBlame>,
    /// Stretches of lines from one commit, as (lines, commit).
    pub(crate) chunks: Vec<(Range<usize>, usize)>,
    /// The chunk under the pointer, and where the pointer was.
    hovered: Option<(usize, Point<Pixels>)>,
}

/// Consecutive lines from the same commit, merged into one stretch each.
pub(crate) fn chunks(lines: &[usize]) -> Vec<(Range<usize>, usize)> {
    let mut out: Vec<(Range<usize>, usize)> = Vec::new();
    for (line, &commit) in lines.iter().enumerate() {
        match out.last_mut() {
            Some((range, c)) if *c == commit && range.end == line => range.end = line + 1,
            _ => out.push((line..line + 1, commit)),
        }
    }
    out
}

/// The blame text for a stretch's first row: author, then age, cut to fit `columns`.
pub(crate) fn caption(commit: &BlameCommit, columns: usize) -> String {
    let age = commit.age.as_str();
    let room = columns.saturating_sub(age.chars().count() + 1);
    let mut author: String = commit.author.chars().take(room).collect();
    if author.chars().count() < commit.author.chars().count() && room > 0 {
        author.pop();
        author.push('…');
    }
    let pad = columns.saturating_sub(author.chars().count() + age.chars().count());
    let mut out = author;
    out.extend(std::iter::repeat_n(' ', pad.max(1)));
    out.push_str(age);
    out
}

impl EditorView {
    /// Shows or hides the blame gutter; `None` hides it.
    pub fn set_file_blame(&mut self, blame: Option<GutterBlame>, cx: &mut Context<Self>) {
        if self.file_blame.blame == blame {
            return;
        }
        self.file_blame.chunks = blame.as_ref().map(|b| chunks(&b.lines)).unwrap_or_default();
        self.file_blame.blame = blame;
        self.file_blame.hovered = None;
        cx.notify();
    }

    pub fn has_file_blame(&self) -> bool {
        self.file_blame.blame.is_some()
    }

    /// The blame stretch drawn at a window position, if the pointer is over the blame gutter.
    fn blame_chunk_at(&self, position: Point<Pixels>) -> Option<usize> {
        self.file_blame.blame.as_ref()?;
        let layout = self.layout.as_ref()?;
        if position.x < layout.fold_column.1 || position.x >= layout.text_left {
            return None;
        }
        let line = self
            .char_at_position(position)
            .and_then(|at| Some(self.buf()?.line_of(at)))?;
        self.file_blame
            .chunks
            .iter()
            .position(|(lines, _)| lines.contains(&line))
    }

    /// A click on the blame gutter opens that commit's changes; true if it was there.
    pub(crate) fn click_blame(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some(chunk) = self.blame_chunk_at(position) else {
            return false;
        };
        let commit = self.file_blame.chunks[chunk].1;
        if let Some(c) = self
            .file_blame
            .blame
            .as_ref()
            .and_then(|b| b.commits.get(commit))
        {
            cx.emit(GitGutterEvent::OpenCommit { sha: c.sha.clone() });
        }
        true
    }

    /// Follows the pointer over the blame gutter for the commit popover.
    pub(crate) fn hover_blame(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let hovered = self.blame_chunk_at(position).map(|c| (c, position));
        if hovered.map(|h| h.0) != self.file_blame.hovered.map(|h| h.0) {
            self.file_blame.hovered = hovered;
            cx.notify();
        }
    }

    pub(crate) fn blame_hovered(&self) -> Option<usize> {
        self.file_blame.hovered.map(|h| h.0)
    }

    /// The hovered commit's summary, next to the pointer, as JetBrains annotations show it.
    pub(crate) fn render_blame_hover(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (chunk, at) = self.file_blame.hovered?;
        let blame = self.file_blame.blame.as_ref()?;
        let commit = blame.commits.get(self.file_blame.chunks.get(chunk)?.1)?;
        let t = cx.theme().clone();
        let short = &commit.sha[..commit.sha.len().min(7)];
        let head = format!("{} · {} · {short}", commit.author, commit.age);
        let panel = div()
            .id("blame-hover")
            .occlude()
            .max_w(px(420.))
            .px(px(10.))
            .py(px(6.))
            .flex()
            .flex_col()
            .gap(px(2.))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .font_family(t.typography.ui.clone())
            .text_size(t.typography.caption)
            .child(div().text_color(t.color.content_muted).child(head))
            .child(
                div()
                    .text_color(t.color.content)
                    .child(commit.summary.clone()),
            )
            .child(
                div()
                    .text_color(t.color.content_muted)
                    .child("Click to open this commit's changes"),
            );
        let panel = motion::animate_if(
            t.motion.reduced,
            panel,
            ("blame-hover-open", chunk),
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        );
        Some(
            anchored()
                .position(point(at.x + px(12.), at.y + px(12.)))
                .snap_to_window()
                .child(panel)
                .into_any_element(),
        )
    }
}

impl EditorView {
    fn blame_sha(&self) -> Option<String> {
        Some(self.blame.as_ref()?.commit.as_ref()?.sha.clone())
    }

    /// Follows the pointer over the inline blame caption: resting on it opens the commit card.
    pub(crate) fn hover_inline_blame(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let over = self
            .layout
            .as_ref()
            .and_then(|l| l.inline_blame)
            .is_some_and(|b| b.contains(&position))
            && self.blame_sha().is_some();
        if over {
            self.blame_card.hide_timer = None;
            if std::mem::replace(&mut self.blame_card.over_caption, true) {
                return;
            }
            if self.blame_card.shown.is_none() {
                self.blame_card.rest_timer = Some(cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(CARD_DELAY).await;
                    let _ = this.update(cx, |this, cx| {
                        if let Some(sha) = this.blame_sha() {
                            cx.emit(GitGutterEvent::CommitDetails { sha });
                        }
                    });
                }));
            }
        } else if std::mem::take(&mut self.blame_card.over_caption) {
            self.blame_card.rest_timer = None;
            self.schedule_blame_card_hide(cx);
        }
    }

    /// The pointer left the editor; the card stays only if the pointer went onto it.
    pub(crate) fn blame_card_left(&mut self, cx: &mut Context<Self>) {
        self.blame_card.over_caption = false;
        self.blame_card.rest_timer = None;
        self.schedule_blame_card_hide(cx);
    }

    fn schedule_blame_card_hide(&mut self, cx: &mut Context<Self>) {
        if self.blame_card.shown.is_none() {
            return;
        }
        self.blame_card.hide_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CARD_HIDE_DELAY).await;
            let _ = this.update(cx, |this, cx| {
                if !this.blame_card.over_card {
                    this.hide_blame_card(cx);
                }
            });
        }));
    }

    /// The answer to [`GitGutterEvent::CommitDetails`]; dropped if the pointer moved on since.
    pub fn show_commit_details(&mut self, sha: &str, info: CommitInfo, cx: &mut Context<Self>) {
        let card = &self.blame_card;
        if !(card.over_caption || card.over_card) || self.blame_sha().as_deref() != Some(sha) {
            return;
        }
        let card = &mut self.blame_card;
        let (pull, opened) = match card.shown.take() {
            Some(old) if old.sha == sha => (old.pull, old.opened),
            _ => (None, Opening::now()),
        };
        card.shown = Some(CardShown {
            sha: sha.to_string(),
            info,
            pull,
            opened,
        });
        cx.notify();
    }

    /// The pull request found for the commit on show, which adds the "Open pull request" button.
    pub fn show_commit_pr(&mut self, sha: &str, number: u64, cx: &mut Context<Self>) {
        if let Some(shown) = self.blame_card.shown.as_mut().filter(|s| s.sha == sha) {
            shown.pull = Some(number);
            cx.notify();
        }
    }

    /// Closes the card; true if it was open.
    pub(crate) fn hide_blame_card(&mut self, cx: &mut Context<Self>) -> bool {
        let card = &mut self.blame_card;
        card.rest_timer = None;
        card.hide_timer = None;
        card.over_card = false;
        card.over_caption = false;
        let was_shown = card.shown.take().is_some();
        if was_shown {
            cx.notify();
        }
        was_shown
    }

    pub(crate) fn render_blame_card(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let shown = self.blame_card.shown.as_ref()?;
        let anchor: Bounds<Pixels> = self.layout.as_ref()?.inline_blame?;
        let t = cx.theme().clone();
        let short = &shown.sha[..shown.sha.len().min(7)];
        let button = |id: &'static str, label: SharedString| {
            div()
                .id(id)
                .px(px(8.))
                .py(px(3.))
                .rounded(t.shape.radius_control)
                .cursor_pointer()
                .text_color(t.color.accent)
                .hover(|s| s.bg(t.color.surface_hover))
                .child(label)
        };
        let sha = shown.sha.clone();
        let open = button("blame-card-open", "Open changes".into()).on_click(cx.listener({
            let sha = sha.clone();
            move |this, _: &ClickEvent, _, cx| {
                cx.emit(GitGutterEvent::OpenCommit { sha: sha.clone() });
                this.hide_blame_card(cx);
            }
        }));
        let pull = shown.pull.map(|number| {
            button(
                "blame-card-pr",
                format!("Open pull request #{number}").into(),
            )
            .on_click(cx.listener({
                let sha = sha.clone();
                move |this, _: &ClickEvent, _, cx| {
                    cx.emit(GitGutterEvent::OpenPullRequest { sha: sha.clone() });
                    this.hide_blame_card(cx);
                }
            }))
        });
        let copy = button("blame-card-copy", "Copy SHA".into()).on_click(cx.listener({
            let sha = sha.clone();
            move |this, _: &ClickEvent, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(sha.clone()));
                this.hide_blame_card(cx);
            }
        }));
        let author = self
            .blame
            .as_ref()
            .and_then(|b| b.commit.as_ref())
            .map_or(String::new(), |c| c.author.clone());
        let panel = div()
            .id("blame-card")
            .occlude()
            .w(px(420.))
            .max_h(px(320.))
            .px(px(10.))
            .py(px(8.))
            .flex()
            .flex_col()
            .gap(px(6.))
            .bg(t.color.surface)
            .border_1()
            .border_color(t.color.border)
            .rounded(t.shape.radius_panel)
            .shadow(vec![t.popover_shadow()])
            .font_family(t.typography.ui.clone())
            .text_size(t.typography.caption)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                this.blame_card.over_card = *hovered;
                if *hovered {
                    this.blame_card.hide_timer = None;
                } else {
                    this.schedule_blame_card_hide(cx);
                }
            }))
            .child(
                div()
                    .text_color(t.color.content)
                    .child(format!("{author} <{}>", shown.info.email)),
            )
            .child(
                div()
                    .text_color(t.color.content_muted)
                    .child(format!("{} · {short}", shown.info.date)),
            )
            .child(
                div()
                    .id("blame-card-message")
                    .overflow_y_scroll()
                    .text_color(t.color.content_secondary)
                    .child(shown.info.message.clone()),
            )
            .child(
                div()
                    .flex()
                    .gap(px(4.))
                    .child(open)
                    .children(pull)
                    .child(copy),
            );
        let panel = motion::animate_enter(
            t.motion.reduced,
            shown.opened.running(t.motion.fast),
            panel,
            "blame-card-open-anim",
            Animation::new(t.motion.fast).with_easing(motion::ease_enter()),
            |el, d| el.opacity(d),
        );
        Some(
            deferred(
                anchored()
                    .position(point(anchor.left(), anchor.bottom() + px(2.)))
                    .anchor(Corner::TopLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(panel),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(author: &str, age: &str) -> BlameCommit {
        BlameCommit {
            sha: "0123456789".into(),
            author: author.into(),
            age: age.into(),
            summary: String::new(),
            heat: 0.,
        }
    }

    #[test]
    fn neighbouring_lines_of_one_commit_form_one_stretch() {
        assert_eq!(
            chunks(&[0, 0, 1, 0, 0, 2]),
            vec![(0..2, 0), (2..3, 1), (3..5, 0), (5..6, 2)]
        );
        assert!(chunks(&[]).is_empty());
    }

    #[test]
    fn captions_keep_the_age_and_cut_long_authors() {
        let c = caption(&commit("Ann", "3 days ago"), 20);
        assert_eq!(c, "Ann       3 days ago");
        assert_eq!(c.chars().count(), 20);
        let long = caption(&commit("Zoë Ünal-Long-Name", "2 years ago"), 20);
        assert_eq!(long, "Zoë Üna… 2 years ago");
        assert_eq!(long.chars().count(), 20);
    }
}
