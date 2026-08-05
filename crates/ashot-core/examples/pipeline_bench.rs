//! Replicates the editor's per-mouse-move work to find the drag lag.
//! Run: cargo run --release -p ashot-core --example pipeline_bench

use std::time::Instant;

use ashot_core::{parse_spec, Renderer};
use tiny_skia::{Color, Pixmap};

fn time<R>(label: &str, iters: u32, mut f: impl FnMut() -> R) {
    // Warm-up once (font shaping caches, allocator).
    let _ = f();
    let start = Instant::now();
    for _ in 0..iters {
        let _ = std::hint::black_box(f());
    }
    let per = start.elapsed() / iters;
    println!("{label:44} {:>8.2} ms/iter", per.as_secs_f64() * 1000.0);
}

fn bench_size(w: u32, h: u32) {
    println!("--- image {w}x{h} ---");
    let mut base = Pixmap::new(w, h).unwrap();
    base.fill(Color::WHITE);

    // A realistic editing session: a few committed shapes + the draft.
    let spec = parse_spec(
        r##"[
        {"type":"rect","x":50,"y":50,"w":300,"h":200,"color":"red"},
        {"type":"marker","x":420,"y":100},
        {"type":"text","x":60,"y":300,"text":"note here","size":24},
        {"type":"arrow","from":[500,400],"to":[300,200],"color":"blue"},
        {"type":"rect","x":100,"y":120,"w":250,"h":140,"color":"green"}
        ]"##,
    )
    .unwrap();
    let mut renderer = Renderer::new();

    time("1. base.clone()", 50, || base.clone());

    time("2. renderer.render(5 shapes)", 50, || {
        let mut p = base.clone();
        renderer.render(&mut p, &spec).unwrap();
        p
    });

    time("3. RGBA->BGRA swizzle + copy", 50, || {
        let mut data = base.data().to_vec();
        for px in data.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        data
    });

    time("FULL per-mouse-move pipeline (1+2+3)", 30, || {
        let mut p = base.clone();
        renderer.render(&mut p, &spec).unwrap();
        let mut data = p.data().to_vec();
        for px in data.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
        data
    });
}

fn main() {
    bench_size(800, 400);
    bench_size(1920, 1080);
    println!();
    println!("Mouse move events arrive at 125–1000 Hz depending on the mouse.");
    println!("Every event runs the FULL pipeline synchronously on the UI thread,");
    println!("plus a fresh RenderImage id forces a full GPU texture re-upload.");
}
