use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("screenshot permission denied by the desktop portal; run `ashot setup` once to grant it")]
    PortalDenied,
    #[error("desktop portal unavailable: {0}")]
    PortalUnavailable(String),
    #[error("invalid annotation spec: {0}")]
    InvalidSpec(String),
    #[error("invalid color {0:?}; use #RGB, #RRGGBB, #RRGGBBAA or a basic color name")]
    InvalidColor(String),
    #[error("monitor {0} not found ({1} available)")]
    MonitorNotFound(usize, usize),
    #[error("monitor enumeration requires a Wayland session: {0}")]
    WaylandUnavailable(String),
    #[error("region {0},{1} {2}x{3} is outside the {4}x{5} image")]
    RegionOutOfBounds(i32, i32, u32, u32, u32, u32),
    #[error("image error: {0}")]
    Image(String),
    #[error("clipboard copy failed: {0}")]
    Clipboard(String),
    #[error("screen recording failed: {0}")]
    Record(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Stable machine-readable code for the CLI's JSON error contract.
    pub fn code(&self) -> &'static str {
        match self {
            Error::PortalDenied => "portal_denied",
            Error::PortalUnavailable(_) => "portal_unavailable",
            Error::InvalidSpec(_) => "invalid_spec",
            Error::InvalidColor(_) => "invalid_color",
            Error::MonitorNotFound(..) => "monitor_not_found",
            Error::WaylandUnavailable(_) => "wayland_unavailable",
            Error::RegionOutOfBounds(..) => "region_out_of_bounds",
            Error::Image(_) => "image",
            Error::Clipboard(_) => "clipboard",
            Error::Record(_) => "record",
            Error::Io(_) => "io",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
