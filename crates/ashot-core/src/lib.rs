//! ashot-core: capture + annotation engine shared by the CLI and the GPUI app.
//!
//! Fully headless — no window, no GPU. See DESIGN.md at the repo root for the
//! decisions this implements.

pub mod capture;
pub mod clipboard;
pub mod error;
pub mod paths;
pub mod render;
pub mod spec;

pub use error::{Error, Result};
pub use render::Renderer;
pub use spec::{parse_spec, Annotation, Style};
