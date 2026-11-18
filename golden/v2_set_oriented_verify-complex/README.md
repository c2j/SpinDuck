# 集合化 V2（复杂入口）能力验证 —— purchase 包 `prc_trd_hz_byfund` 100k 三方基准复现包

本目录验证集合化 **V2** 对 purchase 包**复杂入口** `prc_trd_hz_byfund`（按基金汇总成交，
与简单入口 `prc_trd_deal_purchase` 同包）的准入判定、三方性能与数据一致性。
方法论对齐 `golden/v2_set_oriented_verify`（简单入口）与 `golden/pr48_set_oriented_verify`。

## 结论速览（详见 REPORT.md）

- **V2 判定：Skipped(UnsupportedControlFlow)**（batch-atomic 与 strict 同）→ 安全回退
- **Off→On ≈ 0.97×**（无收益）；**SP→Off ≈ 2.1×**（原生存过更快）
- **四方四表 100k 级数据逐行全等**，`p_o_succeed=0`

## 文件清单

```
v2_set_oriented_verify-complex/
├── README.md            ← 本文件
├── REPORT.md            ← 验证报告（判定 / 性能 / 一致性 / 口径）
├── run_bench.sh         ← 一键三方基准（SpinDuck Off/On/Strict + SP 参照，断言一致性）
├── sql/                 ← 补充资产（语料 tables.sql 未含的依赖）
│   ├── extra_tables.sql ← par_oper_venture / dat_fund_xw_venture / dic_stock_kind_sub / jyfy_temp + commision_type 列宽修正
│   ├── v_par_bond.sql   ← v_par_bond 视图（par_sys_bond 投影）
│   ├── routines/        ← 9 个确定性桩函数（fnc_com_get_fund_code 等，双端同源）
│   ├── seeds_params.sql ← 参数种子（partner/seat_para/bond/oper_venture/dic/jyfy_temp）
│   └── gen_detail.sql   ← N 行 mid_yjqs_detail 生成（互质乘子打散分组键 → ~90k 分组）
└── reports/             ← 实测日志（timings.log / consistency.log / 各方 call 输出）
```

## 方法论

1. **同一份语料**：`golden/case_corpus_purchase/sql` 原样复制进临时工程；
   本包 `sql/` 只补语料缺失的依赖（4 表 + 1 视图 + 9 函数桩），SpinDuck 与参照两侧执行同一文件。
2. **同源 100k 数据**：`gen_detail.sql` 一份文件两侧执行（SpinDuck `query --file`，
   参照 `gsql -f`），分组键经互质乘子打散使 `c_get_data` 产生 ~90k 分组，
   压测逐组循环体（费率查询/债券查询/INSERT+COMMIT）。
3. **测时**：SpinDuck `call` CLI 端到端墙钟；参照 gsql 客户端墙钟。单轮。
4. **一致性**：四张目标表（dat_fund_cjqs / dat_fund_xw_venture /
   mid_yjqs_detail 更新列 / qfqscjhbrbb_temp）全列排序逐行比对，
   数值按 Decimal 归一（消除 '.5' vs '0.5' 格式差）。
5. **步数预算**：`--step-budget 100000000`。

## 环境

- Rust stable + cargo（CLI debug 构建）；python3；docker。
- 参照容器：`opengauss/opengauss:7.0.0-RC3.B025`（amd64/Rosetta），库 `hz`；
  缺 LAPACK/OpenBLAS 共享库时按 `golden/v2_set_oriented_verify/README.md` 的
  `/opt/oglibs2` 方案注入（5.0.0 amd64 在 CALL 上 postmaster 自杀，不可作参照）。

## 复现

```bash
# SpinDuck 侧（无需容器即可跑 Off/On/Strict；SP 参照需容器）
SPINDUCK_BIN=/path/to/spinduck N=1000 bash golden/v2_set_oriented_verify-complex/run_bench.sh   # 冒烟
N=100000 bash ...                                                                              # 完整
# 参照容器（首次建库 hz + 装配，约分钟级）
docker exec sd-og70 bash -lc 'export LD_LIBRARY_PATH=/usr/local/opengauss/lib:/opt/oglibs2; \
  /usr/local/opengauss/bin/gsql -U gaussdb -W "$GS_PASSWORD" -d postgres -c "CREATE DATABASE hz;"'
OG_CONTAINER=sd-og70 OG_LIBS=/opt/oglibs2 bash golden/v2_set_oriented_verify-complex/run_bench.sh
```

## 口径与边界

- debug 构建绝对值仅证明方向与量级。
- 桩函数为确定性实现，两侧同源；真实现差异不影响三方一致性结论。
- On/Strict 均 Skipped→回退，故 Off≈On 是"同一代码路径"的必然结果，
  不是"转换后无收益"——报告 §3/§8 已区分。
