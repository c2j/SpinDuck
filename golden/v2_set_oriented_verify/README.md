# 集合化 V2（feat/set-oriented-v2，PR #54）能力验证 —— 100k 三方基准复现包

本目录是集合化 **V2**（离线候选编译 + 区域证明 + 条件提交，`SetOrientedMode` 三态
`off`/`strict`/`batch-atomic`）的独立验证资产，方法论对齐
`golden/pr48_set_oriented_verify`（PR #48 V1 的差分探针 + 100k 三方基准）。

与 PR #48 的关键差异：**V1 中真实 purchase 主过程落在准入面外（Skipped），
V2 已能将其 Converted**——本包在 100k 同源数据下实测三方：

| 方 | 含义 | 执行形态 |
|---|---|---|
| **SP（存过）** | openGauss **7.0.0-RC3** 原生 plpgsql PACKAGE PROCEDURE `CALL` | 编译态 RBAR（V26.5 替代参照口径） |
| **SpinDuck Off** | `--set-oriented=off`，解释器逐行 FETCH+INSERT | 解释态 RBAR |
| **SpinDuck On** | `--set-oriented=batch-atomic`，集合化批写（V2 Converted） | 集合化 |

> `strict` 模式对本过程报 `Skipped(TransactionBoundaryInsideLoop)`，回退原路径，
> 与 Off 同语义——报告中单列判定，不重复计时。

## 文件清单

```
v2_set_oriented_verify/
├── README.md            ← 本文件
├── REPORT.md            ← 验证报告（判定 / 性能 / 一致性 / 口径）
├── run_bench.sh         ← 一键三方基准（SpinDuck Off/On + SP 参照，断言一致性）
└── reports/             ← 实测日志（timings.log / consistency.log / 各方 call 输出）
```

## 方法论

1. **同一份语料**：`golden/case_corpus_purchase` 的 `sql/` 原样复制进临时工程；
   SpinDuck 侧经 `compile --analyze` + `deploy` 装配（视图手工补入计划
   `ddl_tables`——codeweb 计划不含视图文件，见 REPORT §口径）。
2. **同源 100k 数据**：SpinDuck 侧 `generate_series` 造 100000 行
   `mid_yjqs_detail`（全部命中游标过滤 + F001 资金路径，沿用 Part B 口径），
   `COPY TO` CSV 后 `docker cp` 进参照容器 `\copy` 载入——两侧逐字一致
   （脚本断言两侧均 100000 行）。参数表用 `cases/datasets/seed.sql` 原样 INSERT。
3. **测时**：SpinDuck 为 `call` CLI 端到端墙钟（含进程启动/开库/注册固定开销，
   另测 noop 定开销）；参照为 gsql 客户端墙钟（V26 同口径）。
4. **一致性**：三方各跑一次清洁调用，`dat_trd_repurchase` 全列排序导出，
   归一后逐行 diff（含重复行与 NULL）。
5. **步数预算**：十万行游标循环超默认 `DEFAULT_STEP_BUDGET`，`call` 传
   `--step-budget 100000000`（同 Part B）。

## 环境

- Rust stable + cargo（CLI debug 构建）；python3。
- 参照容器：`opengauss/opengauss:7.0.0-RC3.B025`（amd64，经 Rosetta；
  arm64 变体同样适用）。**5.0.0 amd64 在 procedure CALL 上 postmaster 自杀
  （V21.9–V21.12），不可作参照**；7.0.0-RC3 是 V26.5 登记的替代参照。
- 镜像缺 `libopenblas.so.0`/`libgfortran.so.5`/`libquadmath.so.0`/
  `liblapack*.so.3`（LAPACKE）：从 openEuler 22.03 OS 仓 RPM 解出注入
  `/opt/oglibs2`（详见 REPORT §环境）。

## 复现

```bash
# SpinDuck 侧（无需容器即可跑 Off/On；SP 参照需容器）
SPINDUCK_BIN=/path/to/spinduck N=100000 bash golden/v2_set_oriented_verify/run_bench.sh
N=1000 bash ...        # 快速冒烟
# 参照容器（首次初始化约 1–2 分钟，入口脚本处理 initdb 两段式启动）
docker run -d --name sd-og70 --platform linux/amd64 \
  -e GS_PASSWORD=<口令> -e GS_HOST_AUTH_METHOD=md5 \
  -e GS_NODENAME=sd_og70_node -e GS_PORT=5432 \
  -e LD_LIBRARY_PATH=/opt/oglibs2 -v /path/to/oglibs2:/opt/oglibs2:ro \
  -p 127.0.0.1:5435:5432 opengauss/opengauss:7.0.0-RC3.B025
```

## 口径与边界

- debug 构建绝对值仅证明方向与量级，非 release 承诺。
- 参照为 7.0.0-RC3（非常驻门禁的 5.0.0 黄金 pin），绝对耗时不可与黄金基线混比；
  本包所有"对比"均为同机同数据同时点 A/B（V26.5 口径）。
- `--plan`（codeweb 离线候选）路径对本过程报 `Skipped(UnknownObjectDependency)`
  ——离线 schema 快照不含视图对象；`--file` 运行期证明路径正常 Converted。
  该差异如实记录，见 REPORT §5。
- 本包验证的是「V2 准入面内的真实过程：转换判定、等价性与净收益」；
  准入面外行为由 `tests/guard/guard_set_oriented_*` 守护族覆盖。
