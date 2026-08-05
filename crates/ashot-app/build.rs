//! GPUI links a few system libraries dynamically (`-lxkbcommon`, `-lxcb`, …)
//! that most distros ship only as runtime `lib*.so.N` — the plain `lib*.so`
//! symlink lives in `-dev` packages. Instead of requiring those packages,
//! symlink the versioned libraries into OUT_DIR and add it to the link path.

use std::env;
use std::path::{Path, PathBuf};

const LIBS: &[&str] = &[
    "xkbcommon",
    "xkbcommon-x11",
    "xcb",
    "xcb-xkb",
    "xcb-shape",
    "xcb-xfixes",
];

const SEARCH_DIRS: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib/aarch64-linux-gnu",
    "/usr/lib64",
    "/usr/lib",
    "/lib/x86_64-linux-gnu",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("linux") {
        return;
    }
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let mut linked_any = false;

    for lib in LIBS {
        let plain = format!("lib{lib}.so");
        if SEARCH_DIRS.iter().any(|d| Path::new(d).join(&plain).exists()) {
            continue; // dev symlink already present system-wide
        }
        'found: for dir in SEARCH_DIRS {
            for version in ["so.0", "so.1", "so.2", "so.6"] {
                let candidate = Path::new(dir).join(format!("lib{lib}.{version}"));
                if candidate.exists() {
                    let link = out_dir.join(&plain);
                    let _ = std::fs::remove_file(&link);
                    if std::os::unix::fs::symlink(&candidate, &link).is_ok() {
                        linked_any = true;
                    }
                    break 'found;
                }
            }
        }
    }

    if linked_any {
        println!("cargo:rustc-link-search=native={}", out_dir.display());
    }
}
