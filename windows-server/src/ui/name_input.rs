use std::ops::Range;

use gpui::{prelude::*, *};

pub(super) struct NameInput {
    pub text: String,
    focus: FocusHandle,
    selection: Range<usize>,
    marked: Option<Range<usize>>,
    bounds: Option<Bounds<Pixels>>,
    line: Option<ShapedLine>,
    scroll: Pixels,
}

impl NameInput {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            text: String::new(),
            focus: cx.focus_handle(),
            selection: 0..0,
            marked: None,
            bounds: None,
            line: None,
            scroll: px(0.),
        }
    }

    pub fn set(&mut self, text: String, cx: &mut Context<Self>) {
        self.text = text;
        self.selection = self.text.len()..self.text.len();
        self.marked = None;
        cx.notify();
    }

    fn utf8(&self, offset: usize) -> usize {
        let mut units = 0;
        for (index, c) in self.text.char_indices() {
            if units >= offset {
                return index;
            }
            units += c.len_utf16();
        }
        self.text.len()
    }

    fn bytes(&self, range: Range<usize>) -> Range<usize> {
        self.utf8(range.start)..self.utf8(range.end)
    }
    fn units(&self, range: Range<usize>) -> Range<usize> {
        self.text[..range.start].encode_utf16().count()
            ..self.text[..range.end].encode_utf16().count()
    }
}

impl EntityInputHandler for NameInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.bytes(range);
        *actual = Some(self.units(range.clone()));
        Some(self.text[range].into())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.units(self.selection.clone()),
            reversed: false,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.clone().map(|range| self.units(range))
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|range| self.bytes(range))
            .or(self.marked.clone())
            .unwrap_or(self.selection.clone());
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        if self.text[..range.start].chars().count()
            + text.chars().count()
            + self.text[range.end..].chars().count()
            > 80
        {
            return;
        }
        self.text.replace_range(range.clone(), &text);
        self.selection = range.start + text.len()..range.start + text.len();
        self.marked = None;
        cx.notify();
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = range
            .clone()
            .map(|r| self.bytes(r))
            .or(self.marked.clone())
            .unwrap_or(self.selection.clone())
            .start;
        self.replace_text_in_range(range, text, window, cx);
        let end = self.selection.end;
        if end > start {
            self.marked = Some(start..end);
        }
        if let Some(selected) = selected {
            let prefix = self.text[..start].encode_utf16().count();
            self.selection = self.bytes(prefix + selected.start..prefix + selected.end);
        }
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let range = self.bytes(range);
        let line = self.line.as_ref()?;
        Some(Bounds::from_corners(
            point(
                bounds.left() + line.x_for_index(range.start) - self.scroll,
                bounds.top(),
            ),
            point(
                bounds.left() + line.x_for_index(range.end) - self.scroll,
                bounds.bottom(),
            ),
        ))
    }
    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let index = self
            .line
            .as_ref()?
            .closest_index_for_x(position.x - self.bounds?.left() + self.scroll);
        Some(
            self.text[..index.min(self.text.len())]
                .encode_utf16()
                .count(),
        )
    }
}

impl Render for NameInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();
        let focus = self.focus.clone();
        div()
            .id("profile-name")
            .track_focus(&self.focus)
            .w_full()
            .overflow_hidden()
            .px_3()
            .py_2()
            .border_1()
            .border_color(rgb(0x758399))
            .rounded_md()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    focus.focus(window);
                    let offset = this.line.as_ref().zip(this.bounds).map_or(
                        this.text.len(),
                        |(line, bounds)| {
                            line.closest_index_for_x(event.position.x - bounds.left() + this.scroll)
                                .min(this.text.len())
                        },
                    );
                    this.selection = offset..offset;
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let control =
                    event.keystroke.modifiers.control || event.keystroke.modifiers.platform;
                match event.keystroke.key.as_str() {
                    "a" if control => this.selection = 0..this.text.len(),
                    "v" if control => {
                        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                            this.replace_text_in_range(None, &text, window, cx);
                        }
                    }
                    "c" if control => cx.write_to_clipboard(ClipboardItem::new_string(
                        this.text[this.selection.clone()].into(),
                    )),
                    "backspace" => {
                        if this.selection.is_empty() {
                            let end = this.selection.end;
                            this.selection.start = this.text[..end]
                                .char_indices()
                                .next_back()
                                .map_or(0, |(index, _)| index);
                        }
                        this.replace_text_in_range(None, "", window, cx);
                    }
                    "delete" => {
                        if this.selection.is_empty() {
                            let end = this.selection.end;
                            this.selection.end = this.text[end..]
                                .chars()
                                .next()
                                .map_or(end, |c| end + c.len_utf8());
                        }
                        this.replace_text_in_range(None, "", window, cx);
                    }
                    "left" => {
                        let offset = this.text[..this.selection.start]
                            .char_indices()
                            .next_back()
                            .map_or(0, |(index, _)| index);
                        this.selection = offset..offset;
                    }
                    "right" => {
                        let end = this.selection.end;
                        let offset = this.text[end..]
                            .chars()
                            .next()
                            .map_or(end, |c| end + c.len_utf8());
                        this.selection = offset..offset;
                    }
                    "home" => this.selection = 0..0,
                    "end" => this.selection = this.text.len()..this.text.len(),
                    _ => return,
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, cx| {
                        let input = entity.read(cx);
                        window.handle_input(
                            &input.focus,
                            ElementInputHandler::new(bounds, entity.clone()),
                            cx,
                        );
                        let style = window.text_style();
                        let text: SharedString = input.text.clone().into();
                        let line = window.text_system().shape_line(
                            text.clone(),
                            style.font_size.to_pixels(window.rem_size()),
                            &[TextRun {
                                len: text.len(),
                                font: style.font(),
                                color: style.color,
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }],
                            None,
                        );
                        let cursor = line.x_for_index(input.selection.end);
                        let scroll = (cursor - bounds.size.width + px(12.)).max(px(0.));
                        if !input.selection.is_empty() {
                            let start = line.x_for_index(input.selection.start);
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(bounds.left() + start - scroll, bounds.top()),
                                    size(cursor - start, bounds.size.height),
                                ),
                                rgba(0x4389d955),
                            ));
                        }
                        if input.focus.is_focused(window) {
                            let x = bounds.left() + cursor - scroll;
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(x, bounds.top()),
                                    size(px(1.), bounds.size.height),
                                ),
                                style.color,
                            ));
                        }
                        let _ = line.paint(
                            point(bounds.left() - scroll, bounds.top()),
                            window.line_height(),
                            window,
                            cx,
                        );
                        entity.update(cx, |input, _| {
                            input.bounds = Some(bounds);
                            input.line = Some(line);
                            input.scroll = scroll;
                        });
                    },
                )
                .w_full()
                .h(px(22.)),
            )
    }
}
