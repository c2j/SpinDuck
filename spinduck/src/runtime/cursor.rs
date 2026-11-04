//! 游标状态机（设计 §7.3）：OPEN 快照、FETCH 位置对位、属性元数据。

use crate::error::EngineError;
use crate::exec::{Row, RowStream};
use crate::value::SpinValue;

/// 游标属性（AST CursorAttributeKind 对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorAttrKind {
    NotFound,
    Found,
    IsOpen,
    RowCount,
}

/// 运行期游标状态（RowStream 无 Debug，手动实现）。
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
                ..
            } => {
                let row = stream.fetch_many(1).into_iter().next();
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
                fetched, last_hit, ..
            } => match kind {
                CursorAttrKind::IsOpen => SpinValue::Bool(true),
                CursorAttrKind::RowCount => SpinValue::Int(*fetched as i64),
                CursorAttrKind::Found => SpinValue::Bool(*fetched > 0 && *last_hit),
                CursorAttrKind::NotFound => SpinValue::Bool(*fetched > 0 && !*last_hit),
            },
        }
    }

    pub fn is_open(&self) -> bool {
        matches!(self, CursorState::Open { .. })
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
    fn should_report_closed_cursor_attrs() {
        let mut c = CursorState::Closed;
        assert_eq!(c.attr(CursorAttrKind::IsOpen), SpinValue::Bool(false));
        assert!(c.fetch().is_err());
    }
}
