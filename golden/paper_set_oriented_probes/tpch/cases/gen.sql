-- TPC-H 形状的确定性数据（无随机、无外部文件）。规模约为 sf 0.01 的一半：
-- 25 个国家、100 个供应商、1000 个客户、2000 个零件、4000 条供应记录、8000 张订单、约 3 万行订单明细。
-- 取值全部由主键的算术式导出，任意两次生成完全相同；参照约束（明细的供应商必须是该零件的
-- 供应方、订单日期早于发货/提交/签收日期）按 TPC-H 的口径构造。
-- 论文案例集用的是真实 TPC-H sf 0.01；这里只要数据形状相同，函数逻辑与数据来源无关。
-- run_probes.sh 按 `;` 切分逐条经 `spinduck query` 执行，所以本文件每条语句必须以分号结尾、
-- 体内不能出现分号。

INSERT INTO region
SELECT r,
       CASE r WHEN 0 THEN 'AFRICA' WHEN 1 THEN 'AMERICA' WHEN 2 THEN 'ASIA' WHEN 3 THEN 'EUROPE' ELSE 'MIDDLE EAST' END,
       'region ' || r
  FROM generate_series(0, 4) t(r);

INSERT INTO nation
SELECT n,
       CASE n WHEN 0 THEN 'ALGERIA' WHEN 1 THEN 'ARGENTINA' WHEN 2 THEN 'BRAZIL' WHEN 3 THEN 'CANADA' WHEN 4 THEN 'EGYPT'
              WHEN 5 THEN 'ETHIOPIA' WHEN 6 THEN 'FRANCE' WHEN 7 THEN 'GERMANY' WHEN 8 THEN 'INDIA' WHEN 9 THEN 'INDONESIA'
              WHEN 10 THEN 'IRAN' WHEN 11 THEN 'IRAQ' WHEN 12 THEN 'JAPAN' WHEN 13 THEN 'JORDAN' WHEN 14 THEN 'KENYA'
              WHEN 15 THEN 'MOROCCO' WHEN 16 THEN 'MOZAMBIQUE' WHEN 17 THEN 'PERU' WHEN 18 THEN 'CHINA' WHEN 19 THEN 'ROMANIA'
              WHEN 20 THEN 'SAUDI ARABIA' WHEN 21 THEN 'VIETNAM' WHEN 22 THEN 'RUSSIA' WHEN 23 THEN 'UNITED KINGDOM'
              ELSE 'UNITED STATES' END,
       n // 5,
       'nation ' || n
  FROM generate_series(0, 24) t(n);

INSERT INTO supplier
SELECT s,
       'Supplier#' || lpad(CAST(s AS VARCHAR), 9, '0'),
       'address ' || s,
       s % 25,
       'phone',
       CAST(1000 + (s * 37) % 9000 AS DECIMAL(15,2)),
       'comment'
  FROM generate_series(1, 100) t(s);

INSERT INTO customer
SELECT c,
       'Customer#' || lpad(CAST(c AS VARCHAR), 9, '0'),
       'address ' || c,
       c % 25,
       'phone',
       CAST((c * 13) % 500000 / 100.0 - 500 AS DECIMAL(15,2)),
       CASE c % 5 WHEN 0 THEN 'AUTOMOBILE' WHEN 1 THEN 'BUILDING' WHEN 2 THEN 'FURNITURE' WHEN 3 THEN 'HOUSEHOLD' ELSE 'MACHINERY' END,
       'comment'
  FROM generate_series(1, 1000) t(c);

INSERT INTO part
SELECT p,
       'Part#' || lpad(CAST(p AS VARCHAR), 9, '0'),
       'Manufacturer#' || CAST(1 + p % 5 AS VARCHAR),
       'Brand#' || CAST(1 + p % 5 AS VARCHAR) || CAST(1 + p % 5 AS VARCHAR),
       'TYPE ' || CAST(p % 100 AS VARCHAR),
       1 + p % 50,
       'CONTAINER',
       CAST(900 + (p * 17) % 1000 AS DECIMAL(15,2)),
       'comment'
  FROM generate_series(1, 2000) t(p);

-- 每个零件两个供应方（两者的供应方键必不相同：53 不是 100 的倍数）。
INSERT INTO partsupp
SELECT p, 1 + (p * 7 + k * 53) % 100, 100 + (p * 3 + k * 11) % 9000,
       CAST((1 + (p * 5 + k * 29) % 1000) / 100.0 AS DECIMAL(15,2)), 'comment'
  FROM generate_series(1, 2000) t(p), generate_series(0, 1) u(k);

INSERT INTO orders
SELECT k,
       1 + (k * 7) % 1000,
       CASE WHEN k % 4 = 0 THEN 'O' ELSE 'F' END,
       0,
       CAST(DATE '1992-01-01' + INTERVAL (k % 2400) DAY AS DATE),
       CASE k % 5 WHEN 0 THEN '1-URGENT' WHEN 1 THEN '2-HIGH' WHEN 2 THEN '3-MEDIUM' WHEN 3 THEN '4-NOT SPECIFIED' ELSE '5-LOW' END,
       'Clerk#' || lpad(CAST(k % 1000 AS VARCHAR), 3, '0'),
       0,
       'comment'
  FROM generate_series(1, 8000) t(k);

-- 每张订单 1–5 行明细。零件与供应方取自 partsupp，保证「明细的供应方供应该零件」成立。
INSERT INTO lineitem
SELECT o.o_orderkey,
       ps.ps_partkey,
       ps.ps_suppkey,
       g.ln,
       CAST(1 + (o.o_orderkey + g.ln) % 49 AS DECIMAL(15,2)),
       CAST(ROUND(CAST(1 + (o.o_orderkey + g.ln) % 49 AS DECIMAL(15,2)) * p.p_retailprice, 2) AS DECIMAL(15,2)),
       CAST((o.o_orderkey * 3 + g.ln) % 10 / 100.0 AS DECIMAL(15,2)),
       CAST((o.o_orderkey + g.ln * 2) % 8 / 100.0 AS DECIMAL(15,2)),
       'N',
       'O',
       o.o_orderdate + INTERVAL (1 + (o.o_orderkey + g.ln) % 20) DAY,
       o.o_orderdate + INTERVAL (10 + (o.o_orderkey * 2 + g.ln) % 40) DAY,
       o.o_orderdate + INTERVAL (5 + (o.o_orderkey * 3 + g.ln) % 50) DAY,
       'DELIVER IN PERSON',
       CASE (o.o_orderkey + g.ln) % 7 WHEN 0 THEN 'TRUCK' WHEN 1 THEN 'RAIL' WHEN 2 THEN 'AIR' WHEN 3 THEN 'REG AIR' WHEN 4 THEN 'MAIL' WHEN 5 THEN 'SHIP' ELSE 'FOB' END,
       'comment'
  FROM orders o
  JOIN LATERAL generate_series(1, 1 + o.o_orderkey % 5) g(ln) ON TRUE
  JOIN part p
    ON p.p_partkey = 1 + (o.o_orderkey * 13 + g.ln * 101) % 2000
  JOIN (SELECT ps.*, row_number() OVER (PARTITION BY ps_partkey ORDER BY ps_suppkey) AS rn FROM partsupp ps) ps
    ON ps.ps_partkey = p.p_partkey
   AND ps.rn = 1 + (o.o_orderkey + g.ln) % 2;

UPDATE orders o
   SET o_totalprice = s.total
  FROM (SELECT l_orderkey, SUM(l_extendedprice * (1 - l_discount) * (1 + l_tax)) AS total FROM lineitem GROUP BY l_orderkey) s
 WHERE s.l_orderkey = o.o_orderkey;
