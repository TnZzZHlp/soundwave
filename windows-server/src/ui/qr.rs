use gpui::{prelude::*, *};
use qrcodegen::QrCode;

pub(super) fn render(matrix: QrCode) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, (), window, _| {
            window.paint_quad(fill(bounds, rgb(0xffffff)));
            let modules = matrix.size() + 8;
            let scale = (f32::from(bounds.size.width.min(bounds.size.height)) / modules as f32)
                .floor()
                .max(1.);
            let origin = bounds.center()
                - point(
                    px(modules as f32 * scale / 2.),
                    px(modules as f32 * scale / 2.),
                );
            for y in 0..matrix.size() {
                for x in 0..matrix.size() {
                    if matrix.get_module(x, y) {
                        let start =
                            origin + point(px((x + 4) as f32 * scale), px((y + 4) as f32 * scale));
                        window.paint_quad(fill(
                            Bounds::new(start, size(px(scale), px(scale))),
                            rgb(0x000000),
                        ));
                    }
                }
            }
        },
    )
    .size(px(300.))
    .flex_shrink_0()
}
