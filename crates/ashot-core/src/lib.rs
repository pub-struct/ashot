//! ashot-core: capture + annotation engine shared by the CLI and the GPUI app.
//!
//! Fully headless — no window, no GPU. See DESIGN.md at the repo root for the
//! decisions this implements.

pub mod audio;
pub mod capture;
pub mod captions;
pub mod clipboard;
pub mod error;
pub mod micmon;
pub mod paths;
pub mod player;
pub mod record;
pub mod srt;
pub mod video;
pub mod render;
pub mod spec;
pub mod thumbnails;

pub use error::{Error, Result};
pub use render::Renderer;
pub use spec::{parse_spec, Annotation, Style};
