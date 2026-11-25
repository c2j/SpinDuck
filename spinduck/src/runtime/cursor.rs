//! 游标状态机（设计 §7.3）：OPEN 快照、FETCH 位置对位、属性元数据。

use crate::error::EngineError;
use crate::exec::{Row, RowStream};
use crate::value::SpinValue;

/// 游标属性（AST CursorAttributeKind 对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CursorAttrKind {
    NotFound,
    Found,
    IsOpen,
    RowCount,
}

/// 运行期游标状态（RowStream 无 Debug，手动实现；Clone 供 Worker 帧快照，
/// 行数据经 `Arc` 零拷贝共享，`pos` 为各消费者本地位置）。
#[derive(Clone)]
pub enum CursorState {
    Closed,
    Open {
        stream: RowStream,
        /// 快照总行数
        total: usize,
        /// 已 FETCH 行数
        fetched: usize,
        /// 最近一次 FETCH 是否命中（PG：%FOUND/%NOTFOUND 基于此）
        last_hit: bool,
        /// 是否已执行过 FETCH（空游标首取即 NOTFOUND；未取过则两者皆 false）
        attempted: bool,
    },
}

impl CursorState {
    /// OPEN：行流挂载即快照。OPEN 后未 FETCH：%FOUND/%NOTFOUND 均为 false。
    pub fn open(stream: RowStream) -> Self {
        let total = stream.len();
        CursorState::Open {
            stream,
            total,
            fetched: 0,
            last_hit: false,
            attempted: false,
        }
    }

    /// FETCH：返回当前行；流尾返回 None 且目标槽位不覆盖（PG 语义）。
    pub fn fetch(&mut self) -> Result<Option<Row>, EngineError> {
        match self {
            CursorState::Closed => Err(EngineError::invariant(50, "fetch on closed cursor")),
            CursorState::Open {
                stream,
                fetched,
                last_hit,
                attempted,
                ..
            } => {
                let row = stream.fetch_many(1).into_iter().next();
                *attempted = true;
                *last_hit = row.is_some();
                if row.is_some() {
                    *fetched += 1;
                }
                Ok(row)
            }
        }
    }

    /// 游标属性（§7.3.3：属性是表达式，读状态元数据）。
    pub fn attr(&self, kind: CursorAttrKind) -> SpinValue {
        match self {
            CursorState::Closed => match kind {
                CursorAttrKind::IsOpen | CursorAttrKind::NotFound | CursorAttrKind::Found => {
                    SpinValue::Bool(false)
                }
                CursorAttrKind::RowCount => SpinValue::Int(0),
            },
            CursorState::Open {
                fetched,
                last_hit,
                attempted,
                ..
            } => match kind {
                CursorAttrKind::IsOpen => SpinValue::Bool(true),
                CursorAttrKind::RowCount => SpinValue::Int(*fetched as i64),
                CursorAttrKind::Found => SpinValue::Bool(*last_hit),
                // 空游标首次 FETCH 无行即为 NOTFOUND（Oracle/PG 语义）
                CursorAttrKind::NotFound => SpinValue::Bool(*attempted && !*last_hit),
            },
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self, CursorState::Open { .. })
    }

    /// loop-parallel：已 FETCH 行数（Worker `row_idx` 的绝对基址）。
    pub(crate) fn fetched(&self) -> usize {
        match self {
            CursorState::Open { fetched, .. } => *fetched,
            CursorState::Closed => 0,
        }
    }

    /// SO 混合单行重放（so-exception-row-hybrid §3.1）：定位到"下一次
    /// `fetch` 将返回第 `index` 行（0 基）"的前置状态——等价于顺序
    /// FETCH 完前 `index` 行：`fetched=index`、`last_hit = index>0`、
    /// `attempted = index>0`（与 OPEN 后/流尾取空语义对齐）。
    /// 重放随后执行真实 `Fetch` 指令取行，trace/步数/%ROWCOUNT 全走
    /// 原生路径。`index` 越界时等价于流尾取空前置态（`last_hit=false`）。
    pub fn seek_before(&mut self, index: usize) -> Result<(), EngineError> {
        match self {
            CursorState::Closed => Err(EngineError::invariant(50, "seek on closed cursor")),
            CursorState::Open {
                stream,
                total,
                fetched,
                last_hit,
                attempted,
            } => {
                stream.reset();
                let _ = stream.fetch_many(index.min(*total));
                let in_range = index <= *total;
                *fetched = index.min(*total);
                *last_hit = index > 0 && in_range;
                *attempted = index > 0 || !in_range;
                Ok(())
            }
        }
    }

    /// SO 混合物化重绑定（P0-3b）：OPEN 后、首个 FETCH 前，把快照行
    /// 换成 `sd_cur` 物化表序——此后 FETCH/seek_before/shared_rows
    /// 共用同一行序，`__sd_rn` 与 FETCH 位置的绑定由构造保证。
    /// 仅 Open 态生效；消费位置复位为刚 OPEN 的初态。
    pub(crate) fn swap_stream(&mut self, stream: RowStream) {
        if let CursorState::Open {
            stream: s,
            total,
            fetched,
            last_hit,
            attempted,
        } = self
        {
            *total = stream.len();
            *s = stream;
            *fetched = 0;
            *last_hit = false;
            *attempted = false;
        }
    }

    /// loop-parallel：导出物化行的只读共享存储（仅 Open 态；
    /// `pos`/`fetched` 等消费位置仍归本游标独占）。
    pub(crate) fn shared_rows(&self) -> Option<std::sync::Arc<Vec<Row>>> {
        match self {
            CursorState::Open { stream, .. } => Some(stream.shared_rows()),
            CursorState::Closed => None,
        }
    }
}

impl std::fmt::Debug for CursorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CursorState::Closed => write!(f, "CursorState::Closed"),
            CursorState::Open {
                total,
                fetched,
                last_hit,
                ..
            } => write!(
                f,
                "CursorState::Open {{ total: {total}, fetched: {fetched}, last_hit: {last_hit} }}"
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::RowStream;

    fn stream3() -> RowStream {
        RowStream::new(
            vec!["a".into()],
            vec![
                vec![SpinValue::Int(1)],
                vec![SpinValue::Int(2)],
                vec![SpinValue::Int(3)],
            ],
        )
    }

    #[test]
    fn should_fetch_rows_until_exhausted() {
        let mut c = CursorState::open(stream3());
        // OPEN 后未 FETCH：%NOTFOUND = false（GaussDB/PG 语义）
        assert_eq!(c.attr(CursorAttrKind::NotFound), SpinValue::Bool(false));
        let mut n = 0;
        while c.fetch().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 3);
        assert!(c.fetch().unwrap().is_none());
        assert_eq!(c.attr(CursorAttrKind::RowCount), SpinValue::Int(3));
        assert_eq!(c.attr(CursorAttrKind::NotFound), SpinValue::Bool(true));
        assert_eq!(c.attr(CursorAttrKind::Found), SpinValue::Bool(false));
    }

    #[test]
    fn should_seek_before_index_for_hybrid_row_replay() {
        // SO 混合单行重放：seek_before(e) 等价于"顺序 FETCH 完前 e 行"——
        // 随后真实 Fetch 返回第 e 行；属性面 %ROWCOUNT=e、
        // %FOUND=e>0、%NOTFOUND=false。
        let mut c = CursorState::open(stream3());
        c.seek_before(1).unwrap();
        assert_eq!(c.attr(CursorAttrKind::RowCount), SpinValue::Int(1));
        assert_eq!(c.attr(CursorAttrKind::Found), SpinValue::Bool(true));
        assert_eq!(c.attr(CursorAttrKind::NotFound), SpinValue::Bool(false));
        assert_eq!(c.fetch().unwrap(), Some(vec![SpinValue::Int(2)]));
        assert_eq!(c.attr(CursorAttrKind::RowCount), SpinValue::Int(2));
        // 首行前置态 = OPEN 后未 FETCH：两属性皆 false。
        c.seek_before(0).unwrap();
        assert_eq!(c.attr(CursorAttrKind::NotFound), SpinValue::Bool(false));
        assert_eq!(c.attr(CursorAttrKind::Found), SpinValue::Bool(false));
        assert_eq!(c.fetch().unwrap(), Some(vec![SpinValue::Int(1)]));
        // 流尾前置态（index=total）：等价取空前——NOTFOUND 仍 false，
        // 下一次 fetch 返回 None 且置 NOTFOUND。
        c.seek_before(3).unwrap();
        assert_eq!(c.attr(CursorAttrKind::RowCount), SpinValue::Int(3));
        assert_eq!(c.attr(CursorAttrKind::NotFound), SpinValue::Bool(false));
        assert!(c.fetch().unwrap().is_none());
        assert_eq!(c.attr(CursorAttrKind::NotFound), SpinValue::Bool(true));
        // Closed 态与 fetch 同口径报错。
        assert!(CursorState::Closed.seek_before(0).is_err());
    }

    #[test]
    fn should_report_closed_cursor_attrs() {
        let mut c = CursorState::Closed;
        assert_eq!(c.attr(CursorAttrKind::IsOpen), SpinValue::Bool(false));
        assert!(c.fetch().is_err());
    }

    #[test]
    fn should_export_shared_rows_only_when_open() {
        let mut c = CursorState::open(stream3());
        let rows = c.shared_rows().expect("Open 态应可导出");
        assert_eq!(rows.len(), 3);
        // 导出不改变游标消费位置。
        assert!(c.fetch().unwrap().is_some());
        assert_eq!(c.attr(CursorAttrKind::RowCount), SpinValue::Int(1));
        let closed = CursorState::Closed;
        assert!(closed.shared_rows().is_none());
    }
}
