//! 按名绑定解析（自 `interp.rs` 拆分，AGENTS §3 规模约束；行为一字不改）。

use crate::error::EngineError;
use crate::exec::BindMap;
use crate::runtime::frame::Frame;
use crate::value::SpinValue;

/// 按名绑定表解析：占位符名 → 帧内变量当前值（未定义变量 = ENG 域断言失败，
/// 因为编译期预扫描已保证集合一致）。
pub(crate) fn resolve_binds(
    binds: &[(String, crate::ir::BindSrc)],
    frame: &Frame,
) -> Result<BindMap, EngineError> {
    let mut m = BindMap::new();
    for (ph, src) in binds {
        let v = match src {
            crate::ir::BindSrc::Var(var) => frame.get(var).cloned().ok_or_else(|| {
                EngineError::invariant(44, format!("bind var {} undefined at runtime", var.0))
            })?,
            crate::ir::BindSrc::Field { rec, field } => {
                // 记录字段：帧内 RECORD 按字段名（小写）取值
                let field_key = field.to_ascii_lowercase();
                match frame.get(rec) {
                    Some(SpinValue::Record(fields)) => fields
                        .iter()
                        .find(|(k, _)| k.to_ascii_lowercase() == field_key)
                        .map(|(_, v)| v.clone())
                        .ok_or_else(|| {
                            EngineError::invariant(
                                45,
                                format!("record {} has no field {}", rec.0, field),
                            )
                        })?,
                    Some(other) => {
                        return Err(EngineError::invariant(
                            45,
                            format!("{} is not a record: {:?}", rec.0, other),
                        ));
                    }
                    None => {
                        return Err(EngineError::invariant(
                            44,
                            format!("bind record {} undefined at runtime", rec.0),
                        ));
                    }
                }
            }
        };
        m.insert(ph.clone(), v);
    }
    Ok(m)
}
