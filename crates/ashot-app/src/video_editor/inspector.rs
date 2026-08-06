//! Right-side inspector panel: one body per `Selection` variant.
//!
//! `Selection::None` shows the export panel (caption-burn toggle, output
//! path, Export button/progress) — content moved here from the old toolbar.
//! `Selection::VideoSegment` shows read-only start/end/duration readouts plus
//! Delete/Restore (no numeric-input widget exists anywhere in this codebase
//! today, so per the spec these are display-only labels; editing happens via
//! the existing timeline drag-to-resize / split / delete affordances).
//! `Selection::Zoom` shows the level presets + cx/cy/duration readouts +
//! Remove (this body already existed as a temporary toolbar-adjacent bar in
//! Stage 3; it now lives in its proper place).

use gpui::{div, prelude::*, px, Context, CursorStyle, MouseButton, MouseDownEvent};

use super::state::Selection;
use super::{VideoEditor, ZOOM_LEVELS};
use crate::theme;

pub const INSPECTOR_W: f32 = 260.0;

pub(super) fn render_inspector(
    editor: &VideoEditor,
    cx: &mut Context<VideoEditor>,
) -> impl IntoElement {
    let mut panel = div()
        .id("inspector")
        .flex_none()
        .w(px(INSPECTOR_W))
        .h_full()
        .flex()
        .flex_col()
        .gap_3()
        .p_3()
        .bg(theme::surface())
        .border_l_1()
        .border_color(theme::border())
        .text_color(theme::text_muted());

    panel = match editor.selection {
        Selection::None => panel.child(render_export_body(editor, cx)),
        Selection::VideoSegment(ix) => match editor.state.segments.get(ix) {
            Some(seg) => panel.child(render_segment_body(editor, ix, seg.start, seg.end, seg.removed, cx)),
            None => panel.child(empty_hint()),
        },
        Selection::Zoom(ix) => match editor.state.zooms.get(ix).cloned() {
            Some(z) => panel.child(render_zoom_body(editor, ix, &z, cx)),
            None => panel.child(empty_hint()),
        },
    };
    panel
}

fn empty_hint() -> impl IntoElement {
    div().text_xs().child("Nothing selected.")
}

fn section_title(label: &'static str) -> impl IntoElement {
    div().text_xs().text_color(theme::text_muted()).child(label)
}

fn row(label: &'static str, value: String) -> impl IntoElement {
    div()
        .flex()
        .flex_row()
        .justify_between()
        .text_sm()
        .child(div().text_color(theme::text_muted()).child(label))
        .child(div().child(value))
}

fn render_export_body(editor: &VideoEditor, cx: &mut Context<VideoEditor>) -> impl IntoElement {
    let stem = editor.path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let output = editor.path.with_file_name(format!("{stem}-edited.mp4"));

    let mut body = div().flex().flex_col().gap_3().child(section_title("Export"));

    body = body.child(editor.chip(
        ("cc", 0),
        match (&editor.srt, editor.state.burn_cc) {
            (Some(_), true) => "Captions: burn ✓".into(),
            (Some(_), false) => "Captions: off".into(),
            (None, _) => "Generate captions".into(),
        },
        editor.srt.is_some() && editor.state.burn_cc,
        cx,
        |this, cx| {
            if this.srt.is_some() {
                this.state.burn_cc = !this.state.burn_cc;
                this.history.commit(this.state.clone());
                cx.notify();
            } else {
                this.generate_captions(cx);
            }
        },
    ));
    if let Some(status) = &editor.cc_status {
        body = body.child(div().text_xs().child(status.clone()));
    }

    body = body.child(
        div()
            .text_xs()
            .overflow_hidden()
            .child(format!("→ {}", output.display())),
    );

    body = body.child(
        div()
            .id("export")
            .px_3()
            .py_1p5()
            .rounded_md()
            .cursor(CursorStyle::PointingHand)
            .bg(theme::accent())
            .text_color(gpui::rgb(0xffffff))
            .text_sm()
            .text_center()
            .hover(|s| s.bg(theme::accent_hover()))
            .child(if editor.exporting { "Exporting…" } else { "Export" })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| this.export(cx)),
            ),
    );
    if let Some(status) = &editor.status {
        body = body.child(div().text_xs().child(status.clone()));
    }
    body
}

fn render_segment_body(
    editor: &VideoEditor,
    ix: usize,
    start: f64,
    end: f64,
    removed: bool,
    cx: &mut Context<VideoEditor>,
) -> impl IntoElement {
    let mut body = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(section_title("Video segment"))
        .child(row("Start", format!("{:.2}s", start)))
        .child(row("End", format!("{:.2}s", end)))
        .child(row("Duration", format!("{:.2}s", end - start)));

    body = body.child(editor.chip(
        ("segtoggle", ix),
        if removed { "Restore".into() } else { "Delete".into() },
        false,
        cx,
        move |this, cx| {
            if removed {
                this.state.restore_segment(ix);
            } else {
                this.state.delete_segment(ix);
            }
            this.history.commit(this.state.clone());
            cx.notify();
        },
    ));
    body
}

fn render_zoom_body(
    editor: &VideoEditor,
    ix: usize,
    z: &ashot_core::video::ZoomPoint,
    cx: &mut Context<VideoEditor>,
) -> impl IntoElement {
    let mut body = div()
        .flex()
        .flex_col()
        .gap_2()
        .child(section_title("Zoom segment"))
        .child(row("Level", format!("{:.2}×", z.level)))
        .child(row("Center", format!("{:.0}, {:.0}", z.cx, z.cy)))
        .child(row("Duration", format!("{:.1}s", z.duration)));

    let mut presets = div().flex().flex_row().gap_1();
    for (i, &(label, level)) in ZOOM_LEVELS.iter().enumerate() {
        let active = (z.level - level).abs() < 0.01;
        presets = presets.child(editor.chip(("zpreset", i), label.into(), active, cx, move |this, cx| {
            this.state.set_zoom_level(ix, level);
            this.history.commit(this.state.clone());
            this.refresh_display(cx);
        }));
    }
    body = body.child(presets);

    body = body.child(editor.chip(("zremove", ix), "Remove".into(), false, cx, move |this, cx| {
        this.state.remove_zoom(ix);
        this.selection = Selection::None;
        this.history.commit(this.state.clone());
        this.refresh_display(cx);
    }));
    body
}
