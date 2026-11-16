use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, Context, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, InspectorElementId,
    IntoElement, KeyBinding, LayoutId, MouseButton, MouseDownEvent, PaintQuad, Pixels, Point,
    Render, ScrollWheelEvent, ShapedLine, SharedString, Style, TextRun, UTF16Selection, Window,
    actions, div, fill, point, prelude::*, px, relative, size,
};

use crate::ActiveTheme;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Home,
        End,
        Paste,
        Submit,
        SubmitBeside,
        Cancel,
        Up,
        Down,
        SelectAll
    ]
);

pub(crate) fn init(cx: &mut App) {
    let ctx = Some("TextInput");
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("cmd-left", Home, ctx),
        KeyBinding::new("cmd-right", End, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("cmd-v", Paste, ctx),
        KeyBinding::new("enter", Submit, ctx),
        KeyBinding::new("cmd-enter", SubmitBeside, ctx),
        KeyBinding::new("escape", Cancel, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("cmd-a", SelectAll, ctx),
    ]);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputEvent {
    Changed,
    Submit,
    /// Cmd+Enter: submit, opening the result to the side.
    SubmitBeside,
    Cancel,
    Up,
    Down,
}

/// Single-line text field drawn in the theme, with IME composition.
pub struct TextInput {
    text: String,
    cursor: usize,
    /// The whole text is selected, so typing or pasting replaces it.
    all_selected: bool,
    marked: Option<Range<usize>>,
    placeholder: SharedString,
    masked: bool,
    focus: FocusHandle,
    /// Each drawn line with the byte its text starts at.
    layout: Vec<(usize, ShapedLine)>,
    bounds: Option<Bounds<Pixels>>,
    /// Set for a box that takes several lines and grows to this many rows before it scrolls.
    max_rows: Option<usize>,
    /// The first line shown when the text has more lines than the box has rows.
    scroll_row: usize,
    /// How far the text is scrolled left so the cursor stays inside the box.
    scroll_x: Pixels,
    /// Takes the parent's full height instead of growing with its lines.
    fill: bool,
    line_h: Pixels,
    rows_shown: usize,
    /// The cursor and text length the box last scrolled to, so a wheel scroll is not undone.
    followed: (usize, usize),
    /// The part of a wheel scroll that is less than a row.
    wheel: Pixels,
}

impl EventEmitter<InputEvent> for TextInput {}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TextInput {
    pub fn new(placeholder: impl Into<SharedString>, cx: &mut Context<Self>) -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            all_selected: false,
            marked: None,
            placeholder: placeholder.into(),
            masked: false,
            focus: cx.focus_handle(),
            layout: Vec::new(),
            bounds: None,
            max_rows: None,
            scroll_row: 0,
            scroll_x: px(0.),
            fill: false,
            line_h: px(0.),
            rows_shown: 1,
            followed: (usize::MAX, 0),
            wheel: px(0.),
        }
    }

    /// Makes the box take several lines: Enter breaks the line, Cmd+Enter submits, and the box
    /// grows to `max_rows` rows before it scrolls.
    pub fn multiline(mut self, max_rows: usize) -> Self {
        self.max_rows = Some(max_rows.max(1));
        self
    }

    /// A multi-line box that takes its parent's full height; the parent must have a definite height.
    pub fn fill_height(mut self) -> Self {
        self.fill = true;
        self.max_rows.get_or_insert(1);
        self
    }

    /// Draws one bullet per character and never hands the text to the system, for passwords.
    pub fn masked(mut self) -> Self {
        self.masked = true;
        self
    }

    /// Byte length the characters in `text[from..to]` take once drawn.
    fn shown_len(&self, from: usize, to: usize) -> usize {
        shown_span(&self.text, from, to, self.masked)
    }

    /// Byte ranges of the text's lines, without their line breaks.
    fn lines(&self) -> Vec<Range<usize>> {
        line_ranges(&self.text, self.max_rows.is_some())
    }

    /// The line holding byte `at`, and `at`'s char column in it.
    fn line_at(&self, at: usize) -> (usize, usize) {
        let lines = self.lines();
        let i = lines.iter().rposition(|l| l.start <= at).unwrap_or(0);
        (i, self.text[lines[i].start..at].chars().count())
    }

    /// Moves to the line above or below, keeping the char column where the line is long enough.
    fn move_line(&mut self, down: bool, cx: &mut Context<Self>) {
        self.deselect(cx);
        let lines = self.lines();
        let (line, col) = self.line_at(self.cursor);
        let target = match down {
            true if line + 1 < lines.len() => line + 1,
            false if line > 0 => line - 1,
            _ => return,
        };
        let r = lines[target].clone();
        self.cursor = self.text[r.clone()]
            .char_indices()
            .nth(col)
            .map_or(r.end, |(i, _)| r.start + i);
        cx.notify();
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn set_text(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.all_selected = false;
        self.marked = None;
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    /// Selects the whole text, as a rename field starts, so the first keystroke replaces it.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.all_selected = !self.text.is_empty();
        self.cursor = self.text.len();
        cx.notify();
    }

    /// The range an edit at the cursor replaces: everything while it is all selected.
    fn at_cursor(&self) -> Range<usize> {
        if self.all_selected {
            0..self.text.len()
        } else {
            self.cursor..self.cursor
        }
    }

    fn deselect(&mut self, cx: &mut Context<Self>) -> bool {
        let was = std::mem::take(&mut self.all_selected);
        if was {
            cx.notify();
        }
        was
    }

    fn edit(&mut self, range: Range<usize>, text: &str, cx: &mut Context<Self>) {
        self.all_selected = false;
        self.text.replace_range(range.clone(), text);
        self.cursor = range.start + text.len();
        cx.emit(InputEvent::Changed);
        cx.notify();
    }

    fn prev_boundary(&self, at: usize) -> usize {
        self.text[..at]
            .char_indices()
            .next_back()
            .map_or(0, |(i, _)| i)
    }

    fn next_boundary(&self, at: usize) -> usize {
        self.text[at..]
            .chars()
            .next()
            .map_or(at, |c| at + c.len_utf8())
    }

    fn to_utf16(&self, byte: usize) -> usize {
        self.text[..byte].encode_utf16().count()
    }

    fn offset_from_utf16(&self, units: usize) -> usize {
        let mut seen = 0;
        for (i, c) in self.text.char_indices() {
            if seen >= units {
                return i;
            }
            seen += c.len_utf16();
        }
        self.text.len()
    }

    fn range_from_utf16(&self, r: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(r.start)..self.offset_from_utf16(r.end)
    }
}

impl TextInput {
    fn click_at(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(bounds) = self.bounds else { return };
        if self.masked || self.marked.is_some() || self.line_h <= px(0.) {
            return;
        }
        self.deselect(cx);
        if self.text.is_empty() {
            self.cursor = 0;
            cx.notify();
            return;
        }
        let row = row_at(
            f32::from(pos.y - bounds.top()),
            f32::from(self.line_h),
            self.scroll_row,
            self.layout.len(),
        );
        let Some((start, line)) = self.layout.get(row) else {
            return;
        };
        let at = start + line.closest_index_for_x(pos.x - bounds.left() + self.scroll_x);
        if at <= self.text.len() && self.text.is_char_boundary(at) {
            self.cursor = at;
            cx.notify();
        }
    }

    fn scroll_by(&mut self, delta: Pixels, cx: &mut Context<Self>) {
        let (rows, rest) = wheel_rows(
            f32::from(self.wheel),
            f32::from(delta),
            f32::from(self.line_h),
        );
        self.wheel = px(rest);
        let max = max_scroll(self.layout.len(), self.rows_shown) as isize;
        self.scroll_row = (self.scroll_row as isize - rows).clamp(0, max) as usize;
        cx.notify();
    }
}

impl Render for TextInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("text-input")
            .key_context("TextInput")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Backspace, _, cx| {
                if this.all_selected {
                    this.edit(0..this.text.len(), "", cx);
                } else if this.cursor > 0 {
                    let start = this.prev_boundary(this.cursor);
                    this.edit(start..this.cursor, "", cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Delete, _, cx| {
                if this.all_selected {
                    this.edit(0..this.text.len(), "", cx);
                } else if this.cursor < this.text.len() {
                    let end = this.next_boundary(this.cursor);
                    this.edit(this.cursor..end, "", cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Left, _, cx| {
                // As in a macOS field, Left on a selection goes to its start.
                this.cursor = match this.deselect(cx) {
                    true => 0,
                    false => this.prev_boundary(this.cursor),
                };
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Right, _, cx| {
                if !this.deselect(cx) {
                    this.cursor = this.next_boundary(this.cursor);
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Home, _, cx| {
                this.deselect(cx);
                let lines = this.lines();
                this.cursor = lines[this.line_at(this.cursor).0].start;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &End, _, cx| {
                this.deselect(cx);
                let lines = this.lines();
                this.cursor = lines[this.line_at(this.cursor).0].end;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &Paste, _, cx| {
                if let Some(text) = cx
                    .read_from_clipboard()
                    .and_then(|c: ClipboardItem| c.text())
                {
                    let pasted = match this.max_rows {
                        Some(_) => text.replace("\r\n", "\n"),
                        None => text.lines().next().unwrap_or_default().to_string(),
                    };
                    this.edit(this.at_cursor(), &pasted, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &Submit, _, cx| match this.max_rows {
                Some(_) => this.edit(this.at_cursor(), "\n", cx),
                None => cx.emit(InputEvent::Submit),
            }))
            .on_action(cx.listener(|_, _: &SubmitBeside, _, cx| cx.emit(InputEvent::SubmitBeside)))
            .on_action(cx.listener(|_, _: &Cancel, _, cx| cx.emit(InputEvent::Cancel)))
            .on_action(cx.listener(|this, _: &Up, _, cx| match this.max_rows {
                Some(_) => this.move_line(false, cx),
                None => cx.emit(InputEvent::Up),
            }))
            .on_action(cx.listener(|this, _: &Down, _, cx| match this.max_rows {
                Some(_) => this.move_line(true, cx),
                None => cx.emit(InputEvent::Down),
            }))
            .w_full()
            .when(self.fill, |d| d.h_full())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, e: &MouseDownEvent, _, cx| this.click_at(e.position, cx)),
            )
            .on_scroll_wheel(cx.listener(|this, e: &ScrollWheelEvent, _, cx| {
                if this.max_rows.is_none() || this.layout.len() <= this.rows_shown {
                    return;
                }
                this.scroll_by(e.delta.pixel_delta(this.line_h).y, cx);
                cx.stop_propagation();
            }))
            .child(InputElement { input: cx.entity() })
    }
}

struct InputElement {
    input: Entity<TextInput>,
}

struct InputFrame {
    lines: Vec<(usize, ShapedLine)>,
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
    scroll_row: usize,
    scroll_x: Pixels,
    rows: usize,
    follow: bool,
}

const BULLET: char = '\u{2022}';

fn shown(text: &str, masked: bool) -> std::borrow::Cow<'_, str> {
    match masked {
        true => text.chars().map(|_| BULLET).collect::<String>().into(),
        false => text.into(),
    }
}

fn shown_utf16(text: &str, masked: bool) -> String {
    match masked {
        true => text.encode_utf16().map(|_| BULLET).collect(),
        false => text.to_owned(),
    }
}

fn shown_span(text: &str, from: usize, to: usize, masked: bool) -> usize {
    shown(&text[from..to], masked).len()
}

/// Byte offset of `b` into the line starting at `at`, or the shown-bullet offset for masked text.
/// Layout starts are stale until the next paint, so a masked slice may not land on a char boundary.
fn line_offset(text: &str, at: usize, b: usize, masked: bool) -> usize {
    let plain = b.saturating_sub(at);
    if !masked {
        return plain;
    }
    let b = b.min(text.len()).max(at);
    text.get(at..b).map_or(plain, |s| shown(s, true).len())
}

/// Byte ranges of `text`'s lines without their breaks; a single-line box has one line.
fn line_ranges(text: &str, multiline: bool) -> Vec<Range<usize>> {
    if !multiline {
        return std::iter::once(0..text.len()).collect();
    }
    let mut out = Vec::new();
    let mut start = 0;
    for (i, _) in text.match_indices('\n') {
        out.push(start..i);
        start = i + 1;
    }
    out.push(start..text.len());
    out
}

/// The first row to show so that `line` is in view in a box of `rows` rows.
fn scrolled_to(scroll: usize, line: usize, rows: usize) -> usize {
    if line < scroll {
        line
    } else if line >= scroll + rows {
        line + 1 - rows
    } else {
        scroll
    }
}

/// Rows a box of `height` shows, at least one.
fn fill_rows(height: f32, line_h: f32) -> usize {
    if !height.is_finite() || !line_h.is_finite() || line_h <= 0. {
        return 1;
    }
    ((height / line_h).floor() as usize).max(1)
}

/// The line under `y` pixels below the box top, clamped to the last line.
fn row_at(y: f32, line_h: f32, scroll_row: usize, lines: usize) -> usize {
    let last = lines.saturating_sub(1);
    if line_h <= 0. {
        return scroll_row.min(last);
    }
    (scroll_row + (y.max(0.) / line_h).floor() as usize).min(last)
}

/// Whole rows in the wheel travel plus the remainder; a positive delta moves the content down.
fn wheel_rows(acc: f32, delta: f32, line_h: f32) -> (isize, f32) {
    if line_h <= 0. {
        return (0, 0.);
    }
    let total = acc + delta;
    let rows = (total / line_h).trunc();
    (rows as isize, total - rows * line_h)
}

fn max_scroll(lines: usize, rows: usize) -> usize {
    lines.saturating_sub(rows)
}

/// The left offset of the visible window that keeps the one-pixel caret in view without scrolling past the end of the line.
fn scrolled_x(scroll: Pixels, cursor_x: Pixels, line_width: Pixels, width: Pixels) -> Pixels {
    let caret = px(1.);
    let max = (line_width + caret - width).max(px(0.));
    let wanted = if cursor_x < scroll {
        cursor_x
    } else if cursor_x + caret > scroll + width {
        cursor_x + caret - width
    } else {
        scroll
    };
    wanted.clamp(px(0.), max)
}

impl IntoElement for InputElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for InputElement {
    type RequestLayoutState = ();
    type PrepaintState = InputFrame;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let input = self.input.read(cx);
        let rows = match input.max_rows {
            Some(max) => input.lines().len().clamp(1, max),
            None => 1,
        };
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = match input.fill {
            true => relative(1.).into(),
            false => (window.line_height() * rows as f32).into(),
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> InputFrame {
        let input = self.input.read(cx);
        let theme = cx.theme();
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let lh = window.line_height();
        let focused = input.focus.is_focused(window);
        let ranges = match input.text.is_empty() {
            true => std::iter::once(0..0).collect(),
            false => input.lines(),
        };
        let rows = match input.fill {
            true => fill_rows(f32::from(bounds.size.height), f32::from(lh)),
            false => input.max_rows.unwrap_or(1),
        };
        let (cursor_line, _) = input.line_at(input.cursor);
        let follow = (input.cursor, input.text.len()) != input.followed;
        let wanted_row = match follow {
            true => scrolled_to(input.scroll_row, cursor_line, rows),
            false => input.scroll_row,
        };
        let scroll_row = wanted_row.min(max_scroll(ranges.len(), rows));
        let mut lines = Vec::new();
        for range in ranges.iter() {
            let (text, color) = if input.text.is_empty() {
                (input.placeholder.to_string(), theme.color.content_disabled)
            } else {
                (
                    shown(&input.text[range.clone()], input.masked).into_owned(),
                    theme.color.content,
                )
            };
            let base = TextRun {
                len: text.len(),
                font: style.font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let marked = input
                .marked
                .as_ref()
                .filter(|m| !input.text.is_empty() && m.start >= range.start && m.end <= range.end)
                .map(|m| {
                    input.shown_len(range.start, m.start)..input.shown_len(range.start, m.end)
                });
            let runs = match marked {
                Some(m) => {
                    let underline = gpui::UnderlineStyle {
                        thickness: px(1.),
                        color: Some(color),
                        wavy: false,
                    };
                    vec![
                        TextRun {
                            len: m.start,
                            ..base.clone()
                        },
                        TextRun {
                            len: m.end - m.start,
                            underline: Some(underline),
                            ..base.clone()
                        },
                        TextRun {
                            len: text.len() - m.end,
                            ..base
                        },
                    ]
                    .into_iter()
                    .filter(|r| r.len > 0)
                    .collect()
                }
                None => vec![base],
            };
            let line = window
                .text_system()
                .shape_line(text.into(), font_size, &runs, None);
            lines.push((range.start, line));
        }
        let cursor_x = |line: &ShapedLine, start: usize| match input.text.is_empty() {
            true => px(0.),
            false => line.x_for_index(input.shown_len(start, input.cursor)),
        };
        let scroll_x = match lines.get(cursor_line) {
            Some((start, line)) if !input.text.is_empty() => scrolled_x(
                input.scroll_x,
                cursor_x(line, *start),
                line.width,
                bounds.size.width,
            ),
            _ => px(0.),
        };
        let mut selection = Vec::new();
        let mut cursor = None;
        for (i, (start, line)) in lines.iter().enumerate() {
            if !(scroll_row..scroll_row + rows).contains(&i) {
                continue;
            }
            let top = bounds.top() + lh * (i as f32 - scroll_row as f32);
            if focused && input.all_selected {
                selection.push(fill(
                    Bounds::new(point(bounds.left() - scroll_x, top), size(line.width, lh)),
                    theme.color.surface_accent,
                ));
            }
            if focused && i == cursor_line {
                cursor = Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_x(line, *start) - scroll_x, top),
                        size(px(1.), lh),
                    ),
                    theme.color.accent,
                ));
            }
        }
        InputFrame {
            lines,
            cursor,
            selection,
            scroll_row,
            scroll_x,
            rows,
            follow,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        frame: &mut InputFrame,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        let lh = window.line_height();
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for quad in frame.selection.drain(..) {
                window.paint_quad(quad);
            }
            for (i, (_, line)) in frame.lines.iter().enumerate() {
                if !(frame.scroll_row..frame.scroll_row + frame.rows).contains(&i) {
                    continue;
                }
                let top = bounds.top() + lh * (i as f32 - frame.scroll_row as f32);
                let _ = line.paint(point(bounds.left() - frame.scroll_x, top), lh, window, cx);
            }
        });
        if let Some(cursor) = frame.cursor.take() {
            window.paint_quad(cursor);
        }
        let (lines, scroll_row, scroll_x) = (
            std::mem::take(&mut frame.lines),
            frame.scroll_row,
            frame.scroll_x,
        );
        let (rows, follow) = (frame.rows, frame.follow);
        self.input.update(cx, |input, _| {
            input.line_h = lh;
            input.rows_shown = rows;
            if follow {
                input.followed = (input.cursor, input.text.len());
            }
            input.layout = lines;
            input.scroll_row = scroll_row;
            input.scroll_x = scroll_x;
            input.bounds = Some(bounds);
        });
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range);
        *actual = Some(self.to_utf16(range.start)..self.to_utf16(range.end));
        Some(shown_utf16(&self.text[range], self.masked))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let at = self.to_utf16(self.cursor);
        let start = if self.all_selected { 0 } else { at };
        Some(UTF16Selection {
            range: start..at,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked
            .as_ref()
            .map(|m| self.to_utf16(m.start)..self.to_utf16(m.end))
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {
        self.marked = None;
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked.take())
            .unwrap_or(self.at_cursor());
        self.marked = None;
        self.edit(range, text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|r| self.range_from_utf16(&r))
            .or(self.marked.clone())
            .unwrap_or(self.at_cursor());
        self.edit(range.clone(), text, cx);
        self.marked = (!text.is_empty()).then(|| range.start..range.start + text.len());
    }

    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (start, end) = (
            self.offset_from_utf16(range.start),
            self.offset_from_utf16(range.end),
        );
        let row = self.layout.iter().rposition(|(at, _)| *at <= start)?;
        let (at, line) = &self.layout[row];
        let lh = match self.line_h > px(0.) {
            true => self.line_h,
            false => {
                let rows = self.layout.len().clamp(1, self.max_rows.unwrap_or(1));
                bounds.size.height / rows as f32
            }
        };
        let top = bounds.top() + lh * (row as f32 - self.scroll_row as f32);
        let x = |b: usize| {
            line.x_for_index(line_offset(&self.text, *at, b, self.masked).min(line.len()))
        };
        Some(Bounds::from_corners(
            point(bounds.left() + x(start) - self.scroll_x, top),
            point(bounds.left() + x(end) - self.scroll_x, top + lh),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_rows_floors_and_never_returns_zero() {
        assert_eq!(fill_rows(100., 20.), 5);
        assert_eq!(fill_rows(19., 20.), 1);
        assert_eq!(fill_rows(0., 20.), 1);
        assert_eq!(fill_rows(100., 0.), 1);
        assert_eq!(fill_rows(f32::NAN, 20.), 1);
    }

    #[test]
    fn a_click_maps_to_the_row_under_it() {
        assert_eq!(row_at(0., 20., 0, 5), 0);
        assert_eq!(row_at(45., 20., 0, 5), 2);
        assert_eq!(row_at(45., 20., 3, 10), 5);
        assert_eq!(row_at(500., 20., 0, 3), 2);
        assert_eq!(row_at(-5., 20., 2, 5), 2);
    }

    #[test]
    fn wheel_rows_keeps_the_remainder() {
        assert_eq!(wheel_rows(0., 50., 20.), (2, 10.));
        assert_eq!(wheel_rows(10., -35., 20.), (-1, -5.));
        assert_eq!(wheel_rows(0., 5., 0.), (0, 0.));
    }

    #[test]
    fn scroll_never_passes_the_last_page() {
        assert_eq!(max_scroll(10, 4), 6);
        assert_eq!(max_scroll(3, 4), 0);
    }

    #[test]
    fn masked_text_is_bullets_same_length() {
        assert_eq!(shown("p\u{e9}w", true), "\u{2022}\u{2022}\u{2022}");
        assert_eq!(shown("", true), "");
        assert_eq!(shown("abc", false), "abc");
    }

    #[test]
    fn masked_offsets_map_bytes_to_bullets() {
        assert_eq!(shown_span("p\u{e4}\u{20ac}", 0, 3, true), 6);
        assert_eq!(shown_span("p\u{e4}\u{20ac}", 0, 6, true), 9);
        assert_eq!(shown_span("p\u{e4}\u{20ac}", 0, 3, false), 3);
    }

    #[test]
    fn bounds_never_panic_on_stale_line_start() {
        let text = "x\n\u{304b}d";
        assert_eq!(line_offset(text, 2, 5, true), 3);
        assert_eq!(line_offset(text, 3, 5, true), 2);
        assert_eq!(line_offset(text, 3, 5, false), 2);
        assert_eq!(line_offset(text, 9, 4, true), 0);
    }

    #[test]
    fn masked_text_for_range_has_one_bullet_per_utf16_unit() {
        assert_eq!(shown_utf16("a\u{1f600}", true).chars().count(), 3);
        assert_eq!(shown_utf16("a\u{1f600}", false), "a\u{1f600}");
    }

    #[test]
    fn a_multiline_box_splits_at_breaks_and_scrolls_the_cursor_into_view() {
        assert_eq!(line_ranges("ab\n\ncd", true), vec![0..2, 3..3, 4..6]);
        assert_eq!(line_ranges("ab\ncd", false), vec![0..5]);
        assert_eq!(line_ranges("", true), vec![0..0]);
        assert_eq!(scrolled_to(0, 7, 6), 2);
        assert_eq!(scrolled_to(4, 1, 6), 1);
        assert_eq!(scrolled_to(2, 5, 6), 2);
    }

    #[test]
    fn the_caret_scrolls_into_view_without_leaving_empty_space() {
        let (w, line) = (px(100.), px(300.));
        assert_eq!(scrolled_x(px(0.), px(50.), line, w), px(0.));
        assert_eq!(scrolled_x(px(0.), px(99.), line, w), px(0.));
        assert_eq!(scrolled_x(px(0.), px(120.), line, w), px(21.));
        assert_eq!(scrolled_x(px(80.), px(30.), line, w), px(30.));
        assert_eq!(scrolled_x(px(150.), px(300.), px(120.), w), px(21.));
        assert_eq!(scrolled_x(px(40.), px(30.), px(60.), w), px(0.));
    }
}
