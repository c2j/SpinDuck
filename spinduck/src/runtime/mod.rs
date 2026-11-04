//! 运行时：帧、求值、解释器。
pub mod cursor;
pub mod frame;
pub mod interp;

pub use cursor::{CursorAttrKind, CursorState};
pub use frame::Frame;
pub use interp::{run, run_with_catalog, run_with_trace, RunOutcome};
