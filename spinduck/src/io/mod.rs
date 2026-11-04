//! I/O 层（§13）：数据源/回写接缝。V1 数据源锁定 GaussDB。
pub mod gauss;

pub use gauss::GaussConn;
