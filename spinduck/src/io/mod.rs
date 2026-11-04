//! I/O 层（§13）：数据源/回写接缝。V1 数据源锁定 GaussDB。
//! 连接器依赖外部驱动，按 feature `gauss` 裁剪（无驱动时引擎仍可纯本地运行）。

#[cfg(feature = "gauss")]
pub mod gauss;

#[cfg(feature = "gauss")]
pub use gauss::GaussConn;
