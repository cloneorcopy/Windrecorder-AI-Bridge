pub mod capture;
pub mod crop;
pub mod ffi;
pub mod ffi_c;
pub mod gate;
pub mod winstate;

pub use capture::{monitor_rect, monitors, Grab, Grabber, Monitor, VirtualDesktop};
pub use crop::{black_bands, masked_copy, paint_rgb, Band, MaskPlan, Tile, Urbl, FALLBACK_URBL};
pub use gate::{ChangeGate, Decision, GateConfig};
pub use winstate::{SessionStatus, Snapshot};
