use std::{cell::Cell, rc::Rc};

use gpui::{prelude::*, *};

use super::{dashboard::Dashboard, model::Command};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SliderKind {
    Volume,
    Group,
    Ceiling,
}

impl SliderKind {
    fn command(self, value: u16) -> Command {
        match self {
            Self::Volume => Command::Volume(value),
            Self::Group => Command::Group(value),
            Self::Ceiling => Command::Ceiling(value),
        }
    }
}

impl Dashboard {
    pub(super) fn button(
        &self,
        id: impl Into<ElementId>,
        label: impl Into<SharedString>,
        enabled: bool,
        command: Command,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let colors = self.colors;
        let keyboard_command = command.clone();
        div()
            .id(id)
            .px_3()
            .py_2()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.panel)
            .text_sm()
            .child(label.into())
            .when(!enabled, |el| el.opacity(0.45))
            .when(enabled, |el| {
                el.cursor_pointer()
                    .tab_index(0)
                    .hover(|el| el.bg(colors.hover))
                    .focus(|el| el.border_color(colors.accent))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.dispatch(command.clone(), window, cx);
                    }))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            cx.stop_propagation();
                            this.dispatch(keyboard_command.clone(), window, cx);
                        }
                    }))
            })
    }

    pub(super) fn slider(
        &self,
        id: &'static str,
        kind: SliderKind,
        value: u16,
        range: (u16, u16),
        enabled: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let (minimum, maximum) = range;
        let fraction =
            f32::from(value.saturating_sub(minimum)) / f32::from((maximum - minimum).max(1));
        let bounds = Rc::new(Cell::new(Bounds::<Pixels>::default()));
        let layout_bounds = bounds.clone();
        let down_bounds = bounds.clone();
        let colors = self.colors;
        let map_position = move |position: Point<Pixels>, bounds: Bounds<Pixels>| {
            let fraction = (f32::from(position.x - bounds.left())
                / f32::from(bounds.size.width).max(1.))
            .clamp(0., 1.);
            minimum + (fraction * f32::from(maximum - minimum)).round() as u16
        };
        div()
            .id(id)
            .w_full()
            .h(px(32.))
            .flex()
            .items_center()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .when(!enabled, |el| el.opacity(0.4))
            .child(
                canvas(
                    move |bounds, _, _| {
                        layout_bounds.set(bounds);
                    },
                    move |bounds, (), window, _| {
                        let track = Bounds::new(
                            point(bounds.left(), bounds.center().y - px(2.)),
                            size(bounds.size.width, px(4.)),
                        );
                        window.paint_quad(fill(track, colors.border));
                        window.paint_quad(fill(
                            Bounds::new(
                                track.origin,
                                size(track.size.width * fraction, track.size.height),
                            ),
                            colors.accent,
                        ));
                        let x = (bounds.size.width - px(12.)) * fraction;
                        window.paint_quad(fill(
                            Bounds::new(
                                point(bounds.left() + x, bounds.center().y - px(7.)),
                                size(px(12.), px(14.)),
                            ),
                            colors.accent,
                        ));
                    },
                )
                .w_full()
                .h_full(),
            )
            .when(enabled, |el| {
                el.cursor_pointer()
                    .tab_index(0)
                    .focus(|el| el.border_color(colors.accent))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            this.dragging = Some(kind);
                            this.dispatch(
                                kind.command(map_position(event.position, down_bounds.get())),
                                window,
                                cx,
                            );
                        }),
                    )
                    .on_mouse_move(
                        cx.listener(move |this, event: &MouseMoveEvent, window, cx| {
                            if !event.dragging() {
                                this.dragging = None;
                            }
                            if this.dragging == Some(kind) {
                                this.dispatch(
                                    kind.command(map_position(event.position, bounds.get())),
                                    window,
                                    cx,
                                );
                            }
                        }),
                    )
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        let next = match event.keystroke.key.as_str() {
                            "left" | "down" => value.saturating_sub(1).max(minimum),
                            "right" | "up" => value.saturating_add(1).min(maximum),
                            "home" => minimum,
                            "end" => maximum,
                            _ => return,
                        };
                        cx.stop_propagation();
                        this.dispatch(kind.command(next), window, cx);
                    }))
            })
    }
}
