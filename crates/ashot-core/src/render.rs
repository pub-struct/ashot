//! Headless annotation rendering: tiny-skia rasterization + cosmic-text shaping.
//! No GPU, no window — this is the path agents hit.

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping, SwashCache, Weight};
use tiny_skia::{
    Color, FillRule, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform,
};

use crate::error::{Error, Result};
use crate::spec::{Annotation, Style};

pub const DEFAULT_COLOR: &str = "#ff3b30";
pub const DEFAULT_STROKE_WIDTH: f32 = 4.0;
pub const DEFAULT_TEXT_SIZE: f32 = 24.0;
pub const DEFAULT_MARKER_RADIUS: f32 = 16.0;

pub fn parse_color(s: &str) -> Result<Color> {
    let named = match s.to_ascii_lowercase().as_str() {
        "red" => Some((255, 59, 48)),
        "orange" => Some((255, 149, 0)),
        "yellow" => Some((255, 204, 0)),
        "green" => Some((52, 199, 89)),
        "blue" => Some((0, 122, 255)),
        "purple" => Some((175, 82, 222)),
        "pink" => Some((255, 45, 85)),
        "black" => Some((0, 0, 0)),
        "white" => Some((255, 255, 255)),
        "gray" | "grey" => Some((142, 142, 147)),
        _ => None,
    };
    if let Some((r, g, b)) = named {
        return Ok(Color::from_rgba8(r, g, b, 255));
    }
    let hex = s.strip_prefix('#').ok_or_else(|| Error::InvalidColor(s.into()))?;
    let parse2 = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16);
    let parse1 = |i: usize| u8::from_str_radix(&hex[i..i + 1], 16).map(|v| v * 17);
    let (r, g, b, a) = match hex.len() {
        3 => (parse1(0), parse1(1), parse1(2), Ok(255)),
        6 => (parse2(0), parse2(2), parse2(4), Ok(255)),
        8 => (parse2(0), parse2(2), parse2(4), parse2(6)),
        _ => return Err(Error::InvalidColor(s.into())),
    };
    match (r, g, b, a) {
        (Ok(r), Ok(g), Ok(b), Ok(a)) => Ok(Color::from_rgba8(r, g, b, a)),
        _ => Err(Error::InvalidColor(s.into())),
    }
}

struct ResolvedStyle {
    color: Color,
    stroke_width: f32,
    fill_opacity: f32,
}

fn resolve(style: &Style) -> Result<ResolvedStyle> {
    Ok(ResolvedStyle {
        color: parse_color(style.color.as_deref().unwrap_or(DEFAULT_COLOR))?,
        stroke_width: style.stroke_width.unwrap_or(DEFAULT_STROKE_WIDTH).max(0.5),
        fill_opacity: style.fill_opacity.unwrap_or(0.0).clamp(0.0, 1.0),
    })
}

/// Owns the font system; reuse across renders (font discovery is the slow part).
pub struct Renderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        Self {
            font_system: FontSystem::new(),
            swash_cache: SwashCache::new(),
        }
    }

    /// Draw all annotations onto the pixmap, in spec order.
    pub fn render(&mut self, pixmap: &mut Pixmap, annotations: &[Annotation]) -> Result<()> {
        for annotation in annotations {
            match annotation {
                Annotation::Rect { x, y, w, h, style, label } => {
                    let st = resolve(style)?;
                    self.draw_box(pixmap, *x, *y, *w, *h, &st, false)?;
                    self.draw_shape_label(pixmap, label.as_deref(), *x, *y, &st)?;
                }
                Annotation::Ellipse { x, y, w, h, style, label } => {
                    let st = resolve(style)?;
                    self.draw_box(pixmap, *x, *y, *w, *h, &st, true)?;
                    self.draw_shape_label(pixmap, label.as_deref(), *x, *y, &st)?;
                }
                Annotation::Arrow { from, to, style, label } => {
                    let st = resolve(style)?;
                    draw_arrow(pixmap, *from, *to, &st);
                    // Label sits at the tail so it never covers what's pointed at.
                    self.draw_shape_label(pixmap, label.as_deref(), from[0], from[1], &st)?;
                }
                Annotation::Marker { x, y, number, size, style } => {
                    let st = resolve(style)?;
                    let radius = size.unwrap_or(DEFAULT_MARKER_RADIUS).max(4.0);
                    self.draw_marker(pixmap, *x, *y, number.unwrap_or(1), radius, &st)?;
                }
                Annotation::Text { x, y, text, size, style } => {
                    let st = resolve(style)?;
                    let size = size.unwrap_or(DEFAULT_TEXT_SIZE).max(4.0);
                    self.draw_text(pixmap, text, *x, *y, size, st.color, Weight::NORMAL);
                }
            }
        }
        Ok(())
    }

    fn draw_box(
        &mut self,
        pixmap: &mut Pixmap,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        st: &ResolvedStyle,
        ellipse: bool,
    ) -> Result<()> {
        let rect = Rect::from_xywh(x, y, w.max(1.0), h.max(1.0))
            .ok_or_else(|| Error::InvalidSpec(format!("bad shape geometry {x},{y} {w}x{h}")))?;
        let path = {
            let mut pb = PathBuilder::new();
            if ellipse {
                pb.push_oval(rect);
            } else {
                pb.push_rect(rect);
            }
            pb.finish()
                .ok_or_else(|| Error::InvalidSpec("degenerate shape path".into()))?
        };
        if st.fill_opacity > 0.0 {
            let mut fill_color = st.color;
            fill_color.set_alpha(st.color.alpha() * st.fill_opacity);
            let paint = paint_with(fill_color);
            pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
        let paint = paint_with(st.color);
        let stroke = Stroke { width: st.stroke_width, ..Stroke::default() };
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
        Ok(())
    }

    fn draw_marker(
        &mut self,
        pixmap: &mut Pixmap,
        x: f32,
        y: f32,
        number: u32,
        radius: f32,
        st: &ResolvedStyle,
    ) -> Result<()> {
        let circle = {
            let mut pb = PathBuilder::new();
            pb.push_oval(
                Rect::from_xywh(x - radius, y - radius, radius * 2.0, radius * 2.0)
                    .ok_or_else(|| Error::InvalidSpec("bad marker geometry".into()))?,
            );
            pb.finish().ok_or_else(|| Error::InvalidSpec("degenerate marker".into()))?
        };
        pixmap.fill_path(
            &circle,
            &paint_with(st.color),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        // White ring so the badge stays visible on same-colored backgrounds.
        let ring = Stroke { width: (radius * 0.12).clamp(1.5, 3.0), ..Stroke::default() };
        pixmap.stroke_path(
            &circle,
            &paint_with(Color::WHITE),
            &ring,
            Transform::identity(),
            None,
        );
        let text = number.to_string();
        let font_size = radius * 1.1;
        let (tw, th) = self.measure_text(&text, font_size, Weight::BOLD);
        self.draw_text(
            pixmap,
            &text,
            x - tw / 2.0,
            y - th / 2.0,
            font_size,
            Color::WHITE,
            Weight::BOLD,
        );
        Ok(())
    }

    /// Label pill: white text on a shape-colored background above the anchor,
    /// flipped below it when there's no room.
    fn draw_shape_label(
        &mut self,
        pixmap: &mut Pixmap,
        label: Option<&str>,
        x: f32,
        y: f32,
        st: &ResolvedStyle,
    ) -> Result<()> {
        let Some(label) = label else { return Ok(()) };
        let font_size = 18.0;
        let pad = 6.0;
        let (tw, th) = self.measure_text(label, font_size, Weight::NORMAL);
        let (bw, bh) = (tw + pad * 2.0, th + pad * 2.0);
        let by = if y - bh - 4.0 >= 0.0 { y - bh - 4.0 } else { y + 4.0 };
        let bx = x.max(0.0);
        let mut bg = st.color;
        bg.set_alpha(1.0);
        if let Some(rect) = Rect::from_xywh(bx, by, bw, bh) {
            let mut pb = PathBuilder::new();
            pb.push_rect(rect);
            if let Some(path) = pb.finish() {
                pixmap.fill_path(
                    &path,
                    &paint_with(bg),
                    FillRule::Winding,
                    Transform::identity(),
                    None,
                );
            }
        }
        self.draw_text(pixmap, label, bx + pad, by + pad, font_size, Color::WHITE, Weight::NORMAL);
        Ok(())
    }

    fn measure_text(&mut self, text: &str, font_size: f32, weight: Weight) -> (f32, f32) {
        let buffer = self.shape(text, font_size, weight);
        let mut width: f32 = 0.0;
        let mut lines = 0usize;
        for run in buffer.layout_runs() {
            width = width.max(run.line_w);
            lines += 1;
        }
        (width, lines.max(1) as f32 * buffer.metrics().line_height)
    }

    /// Draw `text` with its top-left corner at (x, y).
    fn draw_text(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        x: f32,
        y: f32,
        font_size: f32,
        color: Color,
        weight: Weight,
    ) {
        let buffer = self.shape(text, font_size, weight);
        let text_color = cosmic_text::Color::rgba(
            (color.red() * 255.0) as u8,
            (color.green() * 255.0) as u8,
            (color.blue() * 255.0) as u8,
            (color.alpha() * 255.0) as u8,
        );
        let (px, py) = (x as i32, y as i32);
        let width = pixmap.width() as i32;
        let height = pixmap.height() as i32;
        let data = pixmap.data_mut();
        buffer.draw(
            &mut self.font_system,
            &mut self.swash_cache,
            text_color,
            |gx, gy, gw, gh, c| {
                let a = c.a();
                if a == 0 {
                    return;
                }
                for dy in 0..gh as i32 {
                    for dx in 0..gw as i32 {
                        let tx = px + gx + dx;
                        let ty = py + gy + dy;
                        if tx < 0 || ty < 0 || tx >= width || ty >= height {
                            continue;
                        }
                        blend_px(data, (ty * width + tx) as usize * 4, c.r(), c.g(), c.b(), a);
                    }
                }
            },
        );
    }

    fn shape(&mut self, text: &str, font_size: f32, weight: Weight) -> Buffer {
        let metrics = Metrics::new(font_size, (font_size * 1.25).ceil());
        let mut buffer = Buffer::new(&mut self.font_system, metrics);
        buffer.set_size(&mut self.font_system, None, None);
        buffer.set_text(
            &mut self.font_system,
            text,
            Attrs::new().family(Family::SansSerif).weight(weight),
            Shaping::Advanced,
        );
        buffer.shape_until_scroll(&mut self.font_system, false);
        buffer
    }
}

fn paint_with(color: Color) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.set_color(color);
    paint.anti_alias = true;
    paint
}

/// Source-over blend of a straight-alpha color onto premultiplied pixmap data.
fn blend_px(data: &mut [u8], idx: usize, r: u8, g: u8, b: u8, a: u8) {
    let sa = a as u32;
    let inv = 255 - sa;
    let premul = |src: u8, dst: u8| -> u8 {
        ((src as u32 * sa + dst as u32 * inv + 127) / 255) as u8
    };
    data[idx] = premul(r, data[idx]);
    data[idx + 1] = premul(g, data[idx + 1]);
    data[idx + 2] = premul(b, data[idx + 2]);
    data[idx + 3] = ((sa * 255 + data[idx + 3] as u32 * inv + 127) / 255) as u8;
}

fn draw_arrow(pixmap: &mut Pixmap, from: [f32; 2], to: [f32; 2], st: &ResolvedStyle) {
    let (dx, dy) = (to[0] - from[0], to[1] - from[1]);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1.0 {
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    // max-then-min: for tiny arrows the half-length cap must win, and a
    // clamp(10.0, len*0.5) would panic once len < 20 (min > max).
    let head_len = (st.stroke_width * 4.0).max(10.0).min(len * 0.5);
    let head_w = head_len * 0.7;
    // Shaft stops where the head begins so the tip stays sharp.
    let base = (to[0] - ux * head_len, to[1] - uy * head_len);

    let mut pb = PathBuilder::new();
    pb.move_to(from[0], from[1]);
    pb.line_to(base.0, base.1);
    if let Some(path) = pb.finish() {
        let stroke = Stroke {
            width: st.stroke_width,
            line_cap: tiny_skia::LineCap::Round,
            ..Stroke::default()
        };
        pixmap.stroke_path(&path, &paint_with(st.color), &stroke, Transform::identity(), None);
    }

    let (nx, ny) = (-uy, ux);
    let mut pb = PathBuilder::new();
    pb.move_to(to[0], to[1]);
    pb.line_to(base.0 + nx * head_w / 2.0, base.1 + ny * head_w / 2.0);
    pb.line_to(base.0 - nx * head_w / 2.0, base.1 - ny * head_w / 2.0);
    pb.close();
    if let Some(path) = pb.finish() {
        pixmap.fill_path(
            &path,
            &paint_with(st.color),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Copy a sub-rectangle out of a pixmap (premultiplied RGBA rows).
pub fn crop(src: &Pixmap, x: i32, y: i32, w: u32, h: u32) -> Result<Pixmap> {
    let (sw, sh) = (src.width(), src.height());
    if x < 0
        || y < 0
        || w == 0
        || h == 0
        || x as u32 + w > sw
        || y as u32 + h > sh
    {
        return Err(Error::RegionOutOfBounds(x, y, w, h, sw, sh));
    }
    let mut out = Pixmap::new(w, h).ok_or_else(|| Error::Image("crop allocation failed".into()))?;
    let src_data = src.data();
    let out_data = out.data_mut();
    let src_stride = sw as usize * 4;
    let row_bytes = w as usize * 4;
    for row in 0..h as usize {
        let src_off = (y as usize + row) * src_stride + x as usize * 4;
        let dst_off = row * row_bytes;
        out_data[dst_off..dst_off + row_bytes]
            .copy_from_slice(&src_data[src_off..src_off + row_bytes]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::parse_spec;

    fn white_pixmap(w: u32, h: u32) -> Pixmap {
        let mut p = Pixmap::new(w, h).unwrap();
        p.fill(Color::WHITE);
        p
    }

    fn px(p: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let i = (y * p.width() + x) as usize * 4;
        p.data()[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn parses_colors() {
        assert!(parse_color("#f00").is_ok());
        assert!(parse_color("#ff0000").is_ok());
        assert!(parse_color("#ff000080").is_ok());
        assert!(parse_color("red").is_ok());
        assert!(parse_color("#zzz").is_err());
        assert!(parse_color("chartreuse-ish").is_err());
    }

    #[test]
    fn rect_stroke_lands_on_edge() {
        let mut p = white_pixmap(100, 100);
        let spec =
            parse_spec(r##"[{"type":"rect","x":20,"y":20,"w":60,"h":60,"color":"#ff0000"}]"##)
                .unwrap();
        Renderer::new().render(&mut p, &spec).unwrap();
        let edge = px(&p, 20, 50);
        assert!(edge[0] > 200 && edge[1] < 80, "expected red edge, got {edge:?}");
        let center = px(&p, 50, 50);
        assert_eq!(center, [255, 255, 255, 255], "center must stay unfilled");
    }

    #[test]
    fn fill_opacity_tints_interior() {
        let mut p = white_pixmap(100, 100);
        let spec = parse_spec(
            r##"[{"type":"rect","x":10,"y":10,"w":80,"h":80,"color":"#0000ff","fill_opacity":0.5}]"##,
        )
        .unwrap();
        Renderer::new().render(&mut p, &spec).unwrap();
        let center = px(&p, 50, 50);
        assert!(center[2] > 200 && center[0] < 200, "expected blue tint, got {center:?}");
    }

    #[test]
    fn marker_paints_badge() {
        let mut p = white_pixmap(100, 100);
        let spec = parse_spec(r#"[{"type":"marker","x":50,"y":50,"color":"blue"}]"#).unwrap();
        Renderer::new().render(&mut p, &spec).unwrap();
        // Between the number glyph and the badge edge the fill is solid blue.
        let c = px(&p, 50 + 10, 50);
        assert!(c[2] > 150 && c[0] < 150, "expected blue badge, got {c:?}");
    }

    #[test]
    fn arrow_covers_head_and_shaft() {
        let mut p = white_pixmap(100, 100);
        let spec =
            parse_spec(r#"[{"type":"arrow","from":[10,50],"to":[90,50],"color":"black"}]"#).unwrap();
        Renderer::new().render(&mut p, &spec).unwrap();
        let shaft = px(&p, 40, 50);
        assert!(shaft[0] < 100, "expected dark shaft, got {shaft:?}");
        let head = px(&p, 86, 50);
        assert!(head[0] < 100, "expected dark head, got {head:?}");
    }

    #[test]
    fn short_arrow_does_not_panic() {
        // Regression: head-length clamp(10.0, len*0.5) panicked when len < 20.
        let mut p = white_pixmap(50, 50);
        let spec =
            parse_spec(r#"[{"type":"arrow","from":[10,10],"to":[18,14]}]"#).unwrap();
        Renderer::new().render(&mut p, &spec).unwrap();
    }

    #[test]
    fn text_marks_pixels() {
        let mut p = white_pixmap(200, 60);
        let spec = parse_spec(r#"[{"type":"text","x":5,"y":5,"text":"Hello","color":"black"}]"#)
            .unwrap();
        Renderer::new().render(&mut p, &spec).unwrap();
        let dark = p.data().chunks_exact(4).filter(|c| c[0] < 128).count();
        assert!(dark > 20, "expected text pixels, found {dark}");
    }

    #[test]
    fn crop_extracts_region() {
        let mut p = white_pixmap(100, 100);
        let spec =
            parse_spec(r#"[{"type":"rect","x":0,"y":0,"w":100,"h":100,"color":"red","fill_opacity":1.0}]"#)
                .unwrap();
        Renderer::new().render(&mut p, &spec).unwrap();
        let c = crop(&p, 10, 10, 30, 20).unwrap();
        assert_eq!((c.width(), c.height()), (30, 20));
        assert!(crop(&p, 90, 90, 20, 20).is_err());
    }
}
