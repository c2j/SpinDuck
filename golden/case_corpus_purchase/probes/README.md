# 规则探针（项目级）

本用例的规则行为由引擎种子规则覆盖，探针在引擎测试层（`spinduck/src/**` 单测与
`spinduck/tests/`）。当项目需要规则覆盖（`rules/spinduck-rules.toml` 非空）时，
按 §9 规则生命周期在此追加 `*.sql` + 期望文件。
