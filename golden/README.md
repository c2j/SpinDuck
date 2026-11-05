# 黄金用例（golden）

与 GaussDB 基线逐行比对的最小用例集。每个用例是一个目录，包含：

| 文件 | 作用 |
|---|---|
| `schema.sql` | 建表（字段类型取自考题基线，保持类型语义） |
| `seed.sql` | 种子数据（幂等插入前先 DROP/CREATE） |
| `routine.sql` | **被测侧**例程（Oracle/GaussDB PL 语法，由 SpinDuck 编译执行） |
| `routine_gauss.sql` | **参照侧**等价例程（openGauss 可接受的语法形态） |
| `expected.json` | 参照侧跑出的期望结果（提交入库，防基线漂移） |

## 基线如何产生

参照侧基线由 openGauss 5.0.0 容器实时产出，而不是手写：测试
`tests/golden_gauss.rs` 会

1. 在 openGauss 建表/灌数/建参照例程 → 调用 → 读出结果表；
2. 在 SpinDuck（DuckDB 内核）建同名表 → 从 openGauss 物化种子 →
   注册被测例程 → `CALL` → 读出结果表；
3. 断言两侧逐行一致，且与 `expected.json` 一致。

因此 `expected.json` 一旦与实时基线不一致就会失败——它锁住的是"参照实现的
行为"，不是"某次运行的偶然输出"。修订期望值必须说明是**参照侧行为变了**
还是**被测侧行为变了**（AGENTS §4.3 守护测试纪律）。

## 运行

```bash
SPINDUCK_OG_HOST=127.0.0.1 SPINDUCK_OG_PORT=15432 \
SPINDUCK_OG_PASSWORD='…' \
cargo test --features gauss --test golden_gauss -- --nocapture
```

未设置 `SPINDUCK_OG_HOST` 时用例打印 `SKIP` 并返回（不静默通过）。

## 为什么参照侧不用 GaussDB

考题提供的 GaussDB 实例（`121.36.227.23:8000`）在本机不可达（TCP 连接被拒绝），
因此改用同内核血统的 openGauss 5.0.0。该替代的适用边界与差异（如 openGauss
的 `CREATE PROCEDURE` 不接受 PG 风格 `$$` 体）记录在 `docs/validation.md` V10。
