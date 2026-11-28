-- 参照表（oracle）：用来独立检验 SpinDuck 解释执行（Off）的结果是否正确，与集合化无关。
-- 两类来源，不要混读：
--   [论文] 取自 Hirn & Grust 案例集的 *-tr.sql——论文作者把 PL/SQL 函数编译成的纯 SQL。只做了三处适配：
--          去掉 psql 元命令（\timing on）；:invocations 换成定值（与驱动过程一致，300）；
--          PostgreSQL 的 = ANY (ARRAY[…] :: bpchar[]) 换成 IN (…)；输出列改成与结果表一致的列名。其余逐字。
--   [手写] 案例集里 margin / savings / late / global 的 SQL 版本依赖 PostgreSQL 的行数组与复合类型
--          （array_agg(整行)、trade、savings、dated_order），DuckDB 跑不了，由本包按同一语义手写等价 SQL。
-- run_probes.sh 按 `;` 切分逐条执行，每条语句以分号结尾、体内不能出现分号。

-- ================= [论文] service/service-tr.sql =================
CREATE TABLE oracle_goto_service AS
SELECT c.c_custkey AS c_custkey,
  (SELECT "ifresult2".*
    FROM (LATERAL
          (SELECT sum("RTE0"."o_totalprice") AS "sum"
           FROM orders AS "RTE0"
           WHERE "RTE0"."o_custkey" = "custkey") AS "let0"("totalbusiness_1")
          LEFT OUTER JOIN
          (LATERAL
           (SELECT "totalbusiness_1" > (1000000) AS "q4_1") AS "let1"("q4_1")
           LEFT OUTER JOIN
           LATERAL
           ((SELECT 'Platinum' AS "result"
             WHERE NOT "q4_1" IS DISTINCT FROM True)
              UNION ALL
            (SELECT "ifresult6".*
             FROM (LATERAL
                   (SELECT "totalbusiness_1" > (500000) AS "q8_2") AS "let5"("q8_2")
                   LEFT OUTER JOIN
                   LATERAL
                   ((SELECT 'Gold' AS "result"
                     WHERE NOT "q8_2" IS DISTINCT FROM True)
                      UNION ALL
                    (SELECT 'Regular' AS "result"
                     WHERE "q8_2" IS DISTINCT FROM True)
                   ) AS "ifresult6"
                   ON True)
             WHERE "q4_1" IS DISTINCT FROM True)
           ) AS "ifresult2"
           ON True)
          ON True)) AS service_level
FROM   (SELECT *
        FROM   customer
        ORDER BY c_custkey
        LIMIT 300) AS c, LATERAL (SELECT c.c_custkey) AS _(custkey);;

-- ================= [手写] global/global-pl.sql 的等价 SQL（原文件注释里给出的等价式） =================
CREATE TABLE oracle_goto_global AS
SELECT o.o_orderkey AS l_orderkey,
       CASE WHEN (SELECT COUNT(DISTINCT n.n_regionkey)
                    FROM lineitem l, supplier s, nation n
                   WHERE l.l_orderkey = o.o_orderkey
                     AND l.l_suppkey = s.s_suppkey
                     AND s.s_nationkey = n.n_nationkey) > 1
            THEN 1 ELSE 0 END AS long_distance
  FROM (SELECT o_orderkey FROM orders ORDER BY o_orderkey LIMIT 300) o;

-- ================= [论文] ship/ship-tr.sql =================
CREATE TABLE oracle_goto_ship AS
SELECT c.c_custkey AS c_custkey, c.c_name AS c_name,
(
    SELECT "ifresult4".*
    FROM (LATERAL
          (SELECT count(*) AS "count"
                   FROM lineitem AS "RTE0",
                        orders AS "RTE1"
                   WHERE ("RTE0"."l_orderkey" = "RTE1"."o_orderkey"
                          AND
                          "RTE1"."o_custkey" = "custkey"
                          AND
                          "RTE0"."l_shipmode" IN ('RAIL',
                                                           'TRUCK'))
          ) AS "let0"("ground_1")
          LEFT OUTER JOIN
          (LATERAL
           (SELECT count(*) AS "count"
                    FROM lineitem AS "RTE2",
                         orders AS "RTE3"
                    WHERE ("RTE2"."l_orderkey" = "RTE3"."o_orderkey"
                           AND
                           "RTE3"."o_custkey" = "custkey"
                           AND
                           "RTE2"."l_shipmode" IN ('AIR',
                                                            'REG AIR'))
           ) AS "let1"("air_1")
           LEFT OUTER JOIN
           (LATERAL
            (SELECT count(*) AS "count"
                     FROM lineitem AS "RTE4",
                          orders AS "RTE5"
                     WHERE ("RTE4"."l_orderkey" = "RTE5"."o_orderkey"
                            AND
                            "RTE5"."o_custkey" = "custkey"
                            AND
                            "RTE4"."l_shipmode" = 'MAIL')
            ) AS "let2"("mail_1")
            LEFT OUTER JOIN
            (LATERAL
             (SELECT ("ground_1" >= "air_1" AND "ground_1" >= "mail_1") AS "q4_1"
             ) AS "let3"("q4_1")
             LEFT OUTER JOIN
             LATERAL
             ((SELECT 'ground' AS "result"
               WHERE NOT "q4_1" IS DISTINCT FROM True)
                UNION ALL
              (SELECT "ifresult7".*
               FROM (LATERAL
                     (SELECT ("air_1" >= "ground_1" AND "air_1" >= "mail_1") AS "q9_2"
                     ) AS "let6"("q9_2")
                     LEFT OUTER JOIN
                     LATERAL
                     ((SELECT 'air' AS "result"
                       WHERE NOT "q9_2" IS DISTINCT FROM True)
                        UNION ALL
                      (SELECT "ifresult10".*
                       FROM (LATERAL
                             (SELECT ("mail_1" >= "ground_1" AND "mail_1" >= "air_1") AS "q14_3"
                             ) AS "let9"("q14_3")
                             LEFT OUTER JOIN
                             LATERAL
                             ((SELECT 'mail' AS "result"
                               WHERE NOT "q14_3" IS DISTINCT FROM True)
                                UNION ALL
                              (SELECT NULL :: text AS "result"
                               WHERE "q14_3" IS DISTINCT FROM True)
                             ) AS "ifresult10"
                             ON True)
                       WHERE "q9_2" IS DISTINCT FROM True)
                     ) AS "ifresult7"
                     ON True)
               WHERE "q4_1" IS DISTINCT FROM True)
             ) AS "ifresult4"
             ON True)
            ON True)
           ON True)
          ON True)
) AS shipmode
FROM   (SELECT *
        FROM   customer
        ORDER BY c_custkey
        LIMIT 300) AS c, LATERAL (SELECT c_custkey) AS _(custkey);;

-- ================= [手写] late/late-pl.sql 的等价 SQL =================
-- PL/SQL 语义：multi（存在别的供应商的明细）AND blame（本供应商有延迟明细）AND 没有别的供应商的延迟明细。
CREATE TABLE oracle_goto_late AS
SELECT s.s_name, o.o_orderkey,
       CASE WHEN EXISTS (SELECT 1 FROM lineitem l
                          WHERE l.l_orderkey = o.o_orderkey AND l.l_receiptdate > l.l_commitdate
                            AND l.l_suppkey = s.s_suppkey)
             AND NOT EXISTS (SELECT 1 FROM lineitem l
                          WHERE l.l_orderkey = o.o_orderkey AND l.l_receiptdate > l.l_commitdate
                            AND l.l_suppkey <> s.s_suppkey)
             AND EXISTS (SELECT 1 FROM lineitem l
                          WHERE l.l_orderkey = o.o_orderkey AND l.l_suppkey <> s.s_suppkey)
            THEN 1 ELSE 0 END AS kept_waiting
  FROM (SELECT o_orderkey FROM orders WHERE o_orderstatus = 'F' ORDER BY o_orderkey LIMIT 200) o,
       (SELECT s.s_suppkey, s.s_name
          FROM supplier s, nation n
         WHERE s.s_nationkey = n.n_nationkey AND n.n_name = 'GERMANY') s;

-- ================= [手写] margin/margin-pl.sql 的等价 SQL =================
-- 按日期序（并列按订单键）扫描某零件的各张订单：price = 该订单里此零件的最低价；
-- cheapest = 截至当前的运行最小价（并列取后者）；cheapest_order = 取得该最小值的最近一张订单；
-- profit = price - cheapest；margin = 运行最大差价（并列取后者）；卖出订单 = 最近一张达到 margin 的订单。
CREATE TABLE oracle_goto_margin AS
WITH px AS (
  SELECT l.l_partkey, o.o_orderdate, o.o_orderkey,
         MIN(l.l_extendedprice * (1 - l.l_discount) * (1 + l.l_tax)) AS price
    FROM lineitem l, orders o
   WHERE l.l_orderkey = o.o_orderkey AND l.l_partkey BETWEEN 1 AND 100
   GROUP BY 1, 2, 3),
r AS (
  SELECT px.*,
         row_number() OVER (PARTITION BY l_partkey ORDER BY o_orderdate, o_orderkey) AS rn,
         MIN(price) OVER (PARTITION BY l_partkey ORDER BY o_orderdate, o_orderkey
                          ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS cheapest
    FROM px),
c AS (
  SELECT r.*, price - cheapest AS profit,
         MAX(CASE WHEN price = cheapest THEN rn END)
           OVER (PARTITION BY l_partkey ORDER BY rn ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS cheapest_rn
    FROM r),
m AS (
  SELECT c.*, MAX(profit) OVER (PARTITION BY l_partkey ORDER BY rn
                                ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS margin FROM c),
last AS (
  SELECT m.*, MAX(CASE WHEN profit = margin THEN rn END)
           OVER (PARTITION BY l_partkey ORDER BY rn ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS best_rn,
         row_number() OVER (PARTITION BY l_partkey ORDER BY rn DESC) AS rev
    FROM m)
SELECT l.l_partkey AS p_partkey, cb.o_orderkey AS buy_orderkey, bs.o_orderkey AS sell_orderkey, bs.price - bs.cheapest AS margin
  FROM last l
  JOIN c bs ON bs.l_partkey = l.l_partkey AND bs.rn = l.best_rn
  JOIN c cb ON cb.l_partkey = bs.l_partkey AND cb.rn = bs.cheapest_rn
 WHERE l.rev = 1;

-- ================= [手写] savings/savings-pl.sql 的等价 SQL =================
CREATE TABLE oracle_goto_savings AS
WITH li AS (
  SELECT l_orderkey, l_linenumber, l_partkey, l_suppkey, l_quantity
    FROM lineitem
   WHERE l_orderkey IN (SELECT o_orderkey FROM orders WHERE o_orderstatus = 'O' ORDER BY o_orderkey LIMIT 150)),
cur AS (
  SELECT li.*, ps.ps_supplycost AS cur_cost
    FROM li, partsupp ps
   WHERE ps.ps_partkey = li.l_partkey AND ps.ps_suppkey = li.l_suppkey),
mn AS (
  SELECT cur.*,
         (SELECT MIN(ps.ps_supplycost) FROM partsupp ps
           WHERE ps.ps_partkey = cur.l_partkey AND ps.ps_availqty >= cur.l_quantity) AS min_cost
    FROM cur),
nw AS (
  SELECT mn.*,
         (SELECT MIN(ps.ps_suppkey) FROM partsupp ps
           WHERE ps.ps_supplycost = mn.min_cost AND ps.ps_partkey = mn.l_partkey) AS new_supp
    FROM mn)
SELECT l_orderkey AS o_orderkey,
       (1 - SUM(min_cost * l_quantity) / SUM(cur_cost * l_quantity)) * 100 AS savings_pct,
       SUM(CASE WHEN new_supp <> l_suppkey THEN 1 ELSE 0 END) AS changes
  FROM nw
 GROUP BY l_orderkey
 ORDER BY l_orderkey;

-- ================= [手写] Froid §11 q3conditions / discount_taxprice 的等价 SQL =================
CREATE TABLE oracle_q3 AS
SELECT l.l_orderkey, l.l_linenumber,
       CASE WHEN c.c_mktsegment = 'BUILDING'
             AND o.o_orderdate < CAST('1995-03-15' AS DATE)
             AND l.l_shipdate > CAST('1995-03-15' AS DATE)
             AND NOT (l.l_shipdate > CAST('1995-03-15' AS DATE) + INTERVAL 122 DAY)
            THEN 1 ELSE 0 END AS q3,
       l.l_extendedprice * (1 - l.l_discount) * (1 + l.l_tax) AS disc_price
  FROM lineitem l, orders o, customer c
 WHERE l.l_orderkey = o.o_orderkey
   AND o.o_custkey = c.c_custkey
   AND l.l_orderkey % 16 = 0
   AND (1 + (l.l_orderkey * 7) % 1000) % 5 = 1
 ORDER BY l.l_orderkey, l.l_linenumber
 LIMIT 500;

-- ================= [手写] PRISM 谓词提升场景的等价 SQL =================
CREATE TABLE oracle_prism_hoist AS
SELECT l.l_orderkey, l.l_linenumber
  FROM lineitem l
 WHERE l.l_orderkey <= 2000
   AND NOT (l.l_shipdate > CAST('1995-03-15' AS DATE) + INTERVAL 122 DAY)
 ORDER BY l.l_orderkey, l.l_linenumber;
