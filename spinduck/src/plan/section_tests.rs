//! `[[ref]]` / `[[dynamic]]` 的往返与排序。空段省略与旧计划全文在 `tests.rs`。

use super::{PlanDynamic, PlanRef, RunPlan, SCHEMA_VERSION};

fn gap_plan() -> RunPlan {
    super::tests::plan_with_two_routines_and_a_gap()
}

#[test]
fn should_omit_empty_ref_and_dynamic_sections() {
    let text = gap_plan().to_toml_string();
    assert_eq!(SCHEMA_VERSION, 1);
    assert!(text.contains("schema_version = 1\n"), "{text}");
    assert!(!text.contains("[[ref]]"), "{text}");
    assert!(!text.contains("[[dynamic]]"), "{text}");
    let back = RunPlan::from_toml_str(&text).unwrap();
    assert!(back.refs.is_empty());
    assert!(back.dynamic.is_empty());
}

#[test]
fn should_escape_backslash_and_quote_without_adding_control_escapes() {
    let mut plan = gap_plan();
    plan.generator = "keep\\me\"plain".into();
    let text = plan.to_toml_string();
    assert!(
        text.contains("generator = \"keep\\\\me\\\"plain\"\n"),
        "{text}"
    );
    assert!(!text.contains("\\n"), "{text}");
    assert!(!text.contains("\\u"), "{text}");
}

#[test]
fn should_round_trip_refs_in_stable_order() {
    let refs = vec![
        ref_row("m_obj", "routine", "read", "pkg.a", "sql/a.sql:0"),
        ref_row("a_obj", "view", "type", "pkg.a", "sql/a.sql:1"),
        ref_row("a_obj", "sequence", "call", "pkg.a", "sql/a.sql:0"),
        ref_row("a_obj", "routine", "write", "pkg.a", "sql/a.sql:1"),
        ref_row("a_obj", "routine", "call", "pkg.z", "sql/a.sql:1"),
        ref_row("a_obj", "routine", "call", "pkg.m", "sql/a.sql:0"),
        ref_row("zeta", "table", "write", "pkg.b", "sql/z.sql:2"),
    ];
    let mut plan = gap_plan();
    plan.missing.clear();
    plan.refs = refs;
    plan.dynamic = vec![PlanDynamic {
        raw: "PlVariable([\"v_sql\"])".into(),
        from: "pkg.a".into(),
        at: "sql/a.sql:0".into(),
    }];
    let text = plan.to_toml_string();
    let refs = text.split("[[dynamic]]").next().unwrap_or("");
    assert_eq!(
        ref_signatures(refs),
        [
            "routine call pkg.m",
            "routine call pkg.z",
            "routine write pkg.a",
            "sequence call pkg.a",
            "view type pkg.a",
            "routine read pkg.a",
            "table write pkg.b",
        ]
    );
    let back = RunPlan::from_toml_str(&text).unwrap();
    assert_eq!(back.refs[0].from, "pkg.m");
    assert_eq!(back.refs[1].from, "pkg.z");
    assert_eq!(back.refs.last().unwrap().object, "zeta");
    assert_eq!(back.dynamic, plan.dynamic);
    assert!(!text.contains("[[missing]]"), "{text}");
}

#[test]
fn should_round_trip_dynamic_raw_containing_a_newline() {
    let raw = "PlVariable([\"v_sql\"])\n\r\t";
    let mut plan = gap_plan();
    plan.dynamic = vec![dyn_row(raw)];
    let text = plan.to_toml_string();
    assert!(
        text.contains("raw = \"PlVariable([\\\"v_sql\\\"])\\n\\r\\t\"\n"),
        "{text}"
    );
    let back = RunPlan::from_toml_str(&text).unwrap();
    assert_eq!(back.dynamic[0].raw, raw);
}

#[test]
fn should_round_trip_dynamic_raw_containing_other_control_chars() {
    let raw = "x\u{0001}\u{0008}\u{000B}\u{000C}\u{001F}\u{007F}y";
    let mut plan = gap_plan();
    plan.dynamic = vec![dyn_row(raw)];
    let text = plan.to_toml_string();
    assert!(text.contains("\\u0001"), "{text}");
    assert!(text.contains("\\u000B"), "{text}");
    assert!(text.contains("\\u007F"), "{text}");
    let back = RunPlan::from_toml_str(&text).unwrap();
    assert_eq!(back.dynamic[0].raw, raw);
}

#[test]
fn should_reject_illegal_ref_kind_with_env_0030() {
    let err = RunPlan::from_toml_str(&ref_toml("index", "read")).unwrap_err();
    assert_eq!(err.code.to_string(), "ENV-0030");
    assert!(
        err.message.contains("kind") && err.message.contains("index"),
        "{}",
        err.message
    );
}

#[test]
fn should_reject_illegal_ref_role_with_env_0030() {
    let err = RunPlan::from_toml_str(&ref_toml("table", "admin")).unwrap_err();
    assert_eq!(err.code.to_string(), "ENV-0030");
    assert!(
        err.message.contains("role") && err.message.contains("admin"),
        "{}",
        err.message
    );
}

fn ref_signatures(text: &str) -> Vec<String> {
    let mut kind = "";
    let mut role = "";
    let mut out = Vec::new();
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("kind = ") {
            kind = value.trim_matches('"');
        } else if let Some(value) = line.strip_prefix("role = ") {
            role = value.trim_matches('"');
        } else if let Some(value) = line.strip_prefix("from = ") {
            out.push(format!("{kind} {role} {}", value.trim_matches('"')));
        }
    }
    out
}

fn ref_row(object: &str, kind: &str, role: &str, from: &str, at: &str) -> PlanRef {
    PlanRef {
        object: object.into(),
        kind: kind.into(),
        role: role.into(),
        from: from.into(),
        at: at.into(),
    }
}

fn dyn_row(raw: &str) -> PlanDynamic {
    PlanDynamic {
        raw: raw.into(),
        from: "pkg.a".into(),
        at: "sql/a.sql:0".into(),
    }
}

fn ref_toml(kind: &str, role: &str) -> String {
    format!(
        "[plan]\nschema_version = 1\ntarget = \"t\"\ngenerator = \"g\"\n\
         project_dir = \".\"\n\n[[ref]]\nobject = \"o\"\nkind = \"{kind}\"\n\
         role = \"{role}\"\nfrom = \"f\"\nat = \"a.sql:0\"\n"
    )
}
