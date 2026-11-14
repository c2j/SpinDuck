//! I/O 层（§13）：数据源/回写接缝。V1 数据源锁定 GaussDB。
//! 连接器依赖外部驱动，按 feature `gauss` 裁剪（无驱动时引擎仍可纯本地运行）。

#[cfg(feature = "gauss")]
pub mod gauss;

#[cfg(feature = "gauss")]
pub mod sync;

#[cfg(feature = "gauss")]
pub mod sync_tx;

#[cfg(feature = "gauss")]
pub use gauss::{
    materialize_parallel, materialize_parallel_with_progress, GaussConn, MaterializeBatch,
    MaterializeMode, MaterializeOutcome,
};

#[cfg(feature = "gauss")]
pub use sync::{resolve_mode, SyncMode, SyncOutcome};
