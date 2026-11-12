//! 运行帧：变量作用域与游标状态表（表达式求值见 `runtime/eval.rs`）。

use crate::error::EngineError;
use crate::ir::{AssignTarget, VarName};
use crate::value::SpinValue;
use std::collections::BTreeMap;

use super::cursor::{CursorAttrKind, CursorState};

/// 执行帧（单过程实例；PACKAGE 级状态在 M7 引入 PackageState）。
#[derive(Debug, Default)]
pub struct Frame {
    vars: BTreeMap<String, SpinValue>,
    /// 游标状态表（过程局部，§7.3）
    cursors: BTreeMap<String, CursorState>,
}

impl Frame {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn get(&self, name: &VarName) -> Option<&SpinValue> {
        self.vars.get(&name.0)
    }
    pub fn set(&mut self, name: &VarName, v: SpinValue) {
        self.vars.insert(name.0.clone(), v);
    }

    /// 按形参声明把实参写入帧。省略的尾部实参取已转换的默认值。
    pub fn install_params(
        &mut self,
        params: &[crate::catalog::RoutineParam],
        args: &[SpinValue],
        assignment_loose: bool,
    ) -> Result<(), EngineError> {
        for (i, p) in params.iter().enumerate() {
            let raw = match args.get(i) {
                Some(v) => v.clone(),
                None => p.default.clone().unwrap_or(SpinValue::Null),
            };
            let v = crate::value::coerce_for_slot(&raw, p.ty.as_ref(), assignment_loose)?;
            self.set(&VarName(p.name.to_ascii_lowercase()), v);
        }
        Ok(())
    }
    pub fn record_field(&self, rec: &VarName, field: &str) -> Result<&SpinValue, EngineError> {
        match self.vars.get(&rec.0) {
            Some(SpinValue::Record(fields)) => fields
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(field))
                .map(|(_, v)| v)
                .ok_or_else(|| {
                    EngineError::business(120, format!("record {} has no field {}", rec.0, field))
                }),
            Some(_) => Err(EngineError::business(
                121,
                format!("{} is not a record", rec.0),
            )),
            None => Err(EngineError::business(
                122,
                format!("undefined record variable {}", rec.0),
            )),
        }
    }
    pub fn set_record_field(
        &mut self,
        rec: &VarName,
        field: &str,
        v: SpinValue,
    ) -> Result<(), EngineError> {
        match self.vars.get_mut(&rec.0) {
            Some(SpinValue::Record(fields)) => {
                match fields
                    .iter_mut()
                    .find(|(n, _)| n.eq_ignore_ascii_case(field))
                {
                    Some(slot) => slot.1 = v,
                    None => fields.push((field.to_string(), v)),
                }
                Ok(())
            }
            Some(_) => Err(EngineError::business(
                121,
                format!("{} is not a record", rec.0),
            )),
            None => Err(EngineError::business(
                122,
                format!("undefined record variable {}", rec.0),
            )),
        }
    }
    /// FETCH INTO 的 %ROWTYPE 目标：未定义（或非 Record）时按结果 schema 建 RECORD
    /// （设计 §7.3.2：字段名取查询别名）。
    pub fn ensure_record(&mut self, rec: &VarName, schema: &[String]) {
        let need = match self.vars.get(&rec.0) {
            Some(SpinValue::Record(fields)) => fields.len() != schema.len(),
            _ => true,
        };
        if need {
            self.vars.insert(
                rec.0.clone(),
                SpinValue::Record(
                    schema
                        .iter()
                        .map(|c| (c.clone(), SpinValue::Null))
                        .collect(),
                ),
            );
        }
    }

    pub fn assign(&mut self, target: &AssignTarget, v: SpinValue) -> Result<(), EngineError> {
        match target {
            AssignTarget::Var(name) => {
                self.set(name, v);
                Ok(())
            }
            AssignTarget::Field { rec, field } => self.set_record_field(rec, field, v),
        }
    }
    pub fn var_names(&self) -> Vec<String> {
        self.vars.keys().cloned().collect()
    }

    /// OPEN：挂载游标状态（快照语义，§7.3.1）。
    pub fn open_cursor(&mut self, name: &VarName, state: CursorState) {
        self.cursors.insert(name.0.to_ascii_lowercase(), state);
    }

    /// FETCH：取游标状态可变引用。
    pub fn cursor_mut(&mut self, name: &VarName) -> Result<&mut CursorState, EngineError> {
        self.cursors
            .get_mut(&name.0.to_ascii_lowercase())
            .ok_or_else(|| EngineError::business(140, format!("cursor {} is not open", name.0)))
    }

    /// 游标 schema 副本（FETCH INTO %ROWTYPE 预建 RECORD 用）。
    pub fn cursor_schema(&self, name: &VarName) -> Vec<String> {
        match self.cursors.get(&name.0.to_ascii_lowercase()) {
            Some(CursorState::Open { stream, .. }) => stream.schema.clone(),
            _ => Vec::new(),
        }
    }

    /// CLOSE。
    pub fn close_cursor(&mut self, name: &VarName) -> Result<(), EngineError> {
        match self.cursors.get_mut(&name.0.to_ascii_lowercase()) {
            Some(cs) => {
                *cs = CursorState::Closed;
                Ok(())
            }
            None => Err(EngineError::business(
                141,
                format!("cursor {} is not open", name.0),
            )),
        }
    }

    /// 游标属性求值（§7.3.3）。
    pub fn cursor_attr(&self, name: &VarName, kind: CursorAttrKind) -> SpinValue {
        match self.cursors.get(&name.0.to_ascii_lowercase()) {
            Some(cs) => cs.attr(kind),
            None => match kind {
                // 未 OPEN 的游标：%ISOPEN=false，%FOUND/%NOTFOUND=false，%ROWCOUNT=0
                CursorAttrKind::IsOpen => SpinValue::Bool(false),
                CursorAttrKind::NotFound | CursorAttrKind::Found => SpinValue::Bool(false),
                CursorAttrKind::RowCount => SpinValue::Int(0),
            },
        }
    }
}
