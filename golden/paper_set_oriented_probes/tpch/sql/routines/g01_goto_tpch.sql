-- 探针 G.* —— Hirn & Grust「One WITH RECURSIVE is Worth Many GOTOs」（SIGMOD 2021）案例集里
-- 基于 TPC-H 的函数。案例集 = 论文配套仓库 One-WITH-RECURSIVE-is-Worth-Many-GOTOs/Code
-- （master，2026-11-28 下载），18 个真实世界 PL/SQL 函数中的 8 个基于 TPC-H。
-- 原文是 PL/pgSQL（PostgreSQL 11），这里改写成 GaussDB PL/SQL。改动逐条记在函数头：
--   [头]  CREATE FUNCTION … RETURNS … AS $$ … $$ LANGUAGE PLPGSQL → Oracle 式头部；
--   [游]  array_agg + FOREACH IN ARRAY → 游标 FOR 循环（这几处的语义与遍历顺序无关）；
--   [出]  复合类型返回值（trade、savings）→ 带 OUT 参数的过程；返回的数组改为只返回其元素个数；
--   [空]  PostgreSQL 给复合变量赋「空行」得 NULL；Oracle 的 SELECT INTO 无行时抛 NO_DATA_FOUND → 加异常块；
--   [序]  ORDER BY … LIMIT 1 的并列原文不保证次序，这里补 o_orderkey 作次序键使结果可复现。
-- 另两个基于 TPC-H 的函数 pack（位运算 + 二维数组返回）与 schedule（日期区间数组）依赖集合类型，无法改写。
-- 原文函数体保持逐字，只有上述五类改动。

-- service/service-pl.sql：Simhadri 等 ICDE 2014 的 Example 1。无循环。改动：[头]。
CREATE OR REPLACE FUNCTION service_level(p_custkey IN INTEGER) RETURN VARCHAR2 IS
  v_total  NUMBER;
  v_level  VARCHAR2(20);
BEGIN
  SELECT SUM(o.o_totalprice)
    INTO v_total
    FROM orders o
   WHERE o.o_custkey = p_custkey;

  IF v_total > 1000000 THEN
    v_level := 'Platinum';
  ELSIF v_total > 500000 THEN
    v_level := 'Gold';
  ELSE
    v_level := 'Regular';
  END IF;

  RETURN v_level;
END;
/

-- ship/ship-pl.sql：客户偏好运输方式。无循环。改动：[头]；布尔型三路 IF/ELSIF 逐字保留。
CREATE OR REPLACE FUNCTION preferred_shipmode(p_custkey IN INTEGER) RETURN VARCHAR2 IS
  v_ground  NUMBER;
  v_air     NUMBER;
  v_mail    NUMBER;
BEGIN
  SELECT COUNT(*)
    INTO v_ground
    FROM lineitem l, orders o
   WHERE l.l_orderkey = o.o_orderkey
     AND o.o_custkey = p_custkey
     AND l.l_shipmode IN ('RAIL', 'TRUCK');
  SELECT COUNT(*)
    INTO v_air
    FROM lineitem l, orders o
   WHERE l.l_orderkey = o.o_orderkey
     AND o.o_custkey = p_custkey
     AND l.l_shipmode IN ('AIR', 'REG AIR');
  SELECT COUNT(*)
    INTO v_mail
    FROM lineitem l, orders o
   WHERE l.l_orderkey = o.o_orderkey
     AND o.o_custkey = p_custkey
     AND l.l_shipmode = 'MAIL';

  IF v_ground >= v_air AND v_ground >= v_mail THEN
    RETURN 'ground';
  ELSIF v_air >= v_ground AND v_air >= v_mail THEN
    RETURN 'air';
  ELSIF v_mail >= v_ground AND v_mail >= v_air THEN
    RETURN 'mail';
  END IF;
  RETURN NULL;
END;
/

-- global/global-pl.sql：订单是否跨地区采购。改动：[头][游]；boolean 返回值 → 0/1。
-- 原文把地区键聚合进数组再逐个与 regions[1] 比较，等价于「存在与首元素不同的地区键」。
CREATE OR REPLACE FUNCTION long_distance(p_orderkey IN INTEGER) RETURN INTEGER IS
  v_first  INTEGER;
BEGIN
  FOR r IN (SELECT n.n_regionkey
              FROM lineitem l, supplier s, nation n
             WHERE l.l_orderkey = p_orderkey
               AND l.l_suppkey = s.s_suppkey
               AND s.s_nationkey = n.n_nationkey) LOOP
    IF v_first IS NULL THEN
      v_first := r.n_regionkey;
    END IF;
    IF r.n_regionkey <> v_first THEN
      RETURN 1;
    END IF;
  END LOOP;
  RETURN 0;
END;
/

-- late/late-pl.sql：TPC-H Q21 的 PL/SQL 重写——给定供应商与订单，订单是否因该供应商而延迟发货。
-- 改动：[头][游]；boolean 返回值与布尔变量 → 0/1（multi := multi OR … → IF 赋值）。
-- 形态：游标循环 + 两个跨迭代标志 + 嵌套 IF + 提前 RETURN。
CREATE OR REPLACE FUNCTION order_kept_waiting(p_suppkey IN INTEGER, p_orderkey IN INTEGER) RETURN INTEGER IS
  v_blame  NUMBER := 0;   -- is suppkey to blame?
  v_multi  NUMBER := 0;   -- does this order have multiple suppliers?
BEGIN
  FOR li IN (SELECT l.l_suppkey, l.l_receiptdate, l.l_commitdate
               FROM lineitem l
              WHERE l.l_orderkey = p_orderkey) LOOP
    IF li.l_suppkey <> p_suppkey THEN
      v_multi := 1;
    END IF;
    IF li.l_receiptdate > li.l_commitdate THEN
      IF li.l_suppkey <> p_suppkey THEN
        RETURN 0;
      ELSE
        v_blame := 1;
      END IF;
    END IF;
  END LOOP;
  IF v_multi = 1 AND v_blame = 1 THEN
    RETURN 1;
  END IF;
  RETURN 0;
END;
/

-- margin/margin-pl.sql：零件的最佳买卖订单与差价。
-- 改动：[头][出][空][序]。原文 WHILE 循环里每轮用「下一张订单」查询推进（键集翻页），
-- 循环体内维护运行最小价与最大差价；复合变量 dated_order 拆成两个标量，trade 拆成三个 OUT。
CREATE OR REPLACE PROCEDURE goto_margin(
  p_partkey IN INTEGER,
  p_buy     OUT NUMBER,
  p_sell    OUT NUMBER,
  p_margin  OUT NUMBER
) IS
  v_ok              NUMBER;      -- this_order.orderkey
  v_od              DATE;        -- this_order.orderdate
  v_cheapest        NUMBER;
  v_cheapest_order  NUMBER;
  v_price           NUMBER;
  v_profit          NUMBER;
BEGIN
  p_buy := NULL;
  p_sell := NULL;
  p_margin := NULL;
  v_cheapest := NULL;
  v_cheapest_order := NULL;

  BEGIN
    SELECT o.o_orderkey, o.o_orderdate
      INTO v_ok, v_od
      FROM lineitem l, orders o
     WHERE l.l_orderkey = o.o_orderkey
       AND l.l_partkey = p_partkey
     ORDER BY o.o_orderdate, o.o_orderkey
     LIMIT 1;
  EXCEPTION
    WHEN NO_DATA_FOUND THEN
      v_ok := NULL;
      v_od := NULL;
  END;

  WHILE v_ok IS NOT NULL LOOP
    SELECT MIN(l.l_extendedprice * (1 - l.l_discount) * (1 + l.l_tax))
      INTO v_price
      FROM lineitem l
     WHERE l.l_orderkey = v_ok
       AND l.l_partkey = p_partkey;

    v_cheapest := NVL(v_cheapest, v_price);
    IF v_price <= v_cheapest THEN
      v_cheapest := v_price;
      v_cheapest_order := v_ok;
    END IF;

    v_profit := v_price - v_cheapest;
    p_margin := NVL(p_margin, v_profit);
    IF v_profit >= p_margin THEN
      p_buy := v_cheapest_order;
      p_sell := v_ok;
      p_margin := v_profit;
    END IF;

    BEGIN
      SELECT o.o_orderkey, o.o_orderdate
        INTO v_ok, v_od
        FROM lineitem l, orders o
       WHERE l.l_orderkey = o.o_orderkey
         AND l.l_partkey = p_partkey
         AND (o.o_orderdate > v_od OR (o.o_orderdate = v_od AND o.o_orderkey > v_ok))
       ORDER BY o.o_orderdate, o.o_orderkey
       LIMIT 1;
    EXCEPTION
      WHEN NO_DATA_FOUND THEN
        v_ok := NULL;
        v_od := NULL;
    END;
  END LOOP;
END;
/

-- savings/savings-pl.sql：订单换用成本最低供应商后的成本节省百分比。
-- 改动：[头][出][空]。原文的 FOR item IN 1..items 计数循环与循环内三次查表逐字保留；
-- 返回的「换了供应商的明细」数组改为返回其元素个数。
CREATE OR REPLACE PROCEDURE goto_savings(
  p_orderkey IN INTEGER,
  p_pct      OUT NUMBER,
  p_changes  OUT NUMBER
) IS
  v_ok             NUMBER;
  v_items          NUMBER;
  v_l_partkey      NUMBER;
  v_l_suppkey      NUMBER;
  v_l_quantity     NUMBER;
  v_ps_suppkey     NUMBER;
  v_ps_cost        NUMBER;
  v_min_cost       NUMBER;
  v_new_supplier   NUMBER;
  v_total          NUMBER := 0;
  v_new            NUMBER := 0;
  v_changes        NUMBER := 0;
BEGIN
  p_pct := NULL;
  p_changes := NULL;

  BEGIN
    SELECT o.o_orderkey
      INTO v_ok
      FROM orders o
     WHERE o.o_orderkey = p_orderkey;
  EXCEPTION
    WHEN NO_DATA_FOUND THEN
      RETURN;
  END;

  SELECT COUNT(*)
    INTO v_items
    FROM lineitem l
   WHERE l.l_orderkey = p_orderkey;

  FOR item IN 1 .. v_items LOOP
    SELECT l.l_partkey, l.l_suppkey, l.l_quantity
      INTO v_l_partkey, v_l_suppkey, v_l_quantity
      FROM lineitem l
     WHERE l.l_orderkey = p_orderkey
       AND l.l_linenumber = item;

    SELECT ps.ps_suppkey, ps.ps_supplycost
      INTO v_ps_suppkey, v_ps_cost
      FROM partsupp ps
     WHERE ps.ps_partkey = v_l_partkey
       AND ps.ps_suppkey = v_l_suppkey;

    SELECT MIN(ps.ps_supplycost)
      INTO v_min_cost
      FROM partsupp ps
     WHERE ps.ps_partkey = v_l_partkey
       AND ps.ps_availqty >= v_l_quantity;

    SELECT MIN(ps.ps_suppkey)
      INTO v_new_supplier
      FROM partsupp ps
     WHERE ps.ps_supplycost = v_min_cost
       AND ps.ps_partkey = v_l_partkey;

    IF v_new_supplier <> v_ps_suppkey THEN
      v_changes := v_changes + 1;
    END IF;

    v_total := v_total + v_ps_cost * v_l_quantity;
    v_new := v_new + v_min_cost * v_l_quantity;
  END LOOP;

  p_pct := (1 - v_new / v_total) * 100;
  p_changes := v_changes;
END;
/

-- margin 的「仅改一处」版（goto_margin 是忠实版）。唯一改动：推进查询里的日期比较不再绑定
-- DATE 型变量，改为同时取出 o_orderdate 的文本形式，下一轮用 CAST(文本 AS DATE) 比较。
-- 原因：DATE 型 PL 变量绑定进 SQL 时被渲染成 Date32(n) 文本，内核报
-- KERNEL-0010 invalid date field format（属引擎缺口，见探针 G.margin.fn）。函数体其余部分逐字。
CREATE OR REPLACE PROCEDURE goto_margin_r(
  p_partkey IN INTEGER,
  p_buy     OUT NUMBER,
  p_sell    OUT NUMBER,
  p_margin  OUT NUMBER
) IS
  v_ok              NUMBER;
  v_od              VARCHAR2(10);
  v_cheapest        NUMBER;
  v_cheapest_order  NUMBER;
  v_price           NUMBER;
  v_profit          NUMBER;
BEGIN
  p_buy := NULL;
  p_sell := NULL;
  p_margin := NULL;
  v_cheapest := NULL;
  v_cheapest_order := NULL;

  BEGIN
    SELECT o.o_orderkey, TO_CHAR(o.o_orderdate, 'YYYY-MM-DD')
      INTO v_ok, v_od
      FROM lineitem l, orders o
     WHERE l.l_orderkey = o.o_orderkey
       AND l.l_partkey = p_partkey
     ORDER BY o.o_orderdate, o.o_orderkey
     LIMIT 1;
  EXCEPTION
    WHEN NO_DATA_FOUND THEN
      v_ok := NULL;
      v_od := NULL;
  END;

  WHILE v_ok IS NOT NULL LOOP
    SELECT MIN(l.l_extendedprice * (1 - l.l_discount) * (1 + l.l_tax))
      INTO v_price
      FROM lineitem l
     WHERE l.l_orderkey = v_ok
       AND l.l_partkey = p_partkey;

    v_cheapest := NVL(v_cheapest, v_price);
    IF v_price <= v_cheapest THEN
      v_cheapest := v_price;
      v_cheapest_order := v_ok;
    END IF;

    v_profit := v_price - v_cheapest;
    p_margin := NVL(p_margin, v_profit);
    IF v_profit >= p_margin THEN
      p_buy := v_cheapest_order;
      p_sell := v_ok;
      p_margin := v_profit;
    END IF;

    BEGIN
      SELECT o.o_orderkey, TO_CHAR(o.o_orderdate, 'YYYY-MM-DD')
        INTO v_ok, v_od
        FROM lineitem l, orders o
       WHERE l.l_orderkey = o.o_orderkey
         AND l.l_partkey = p_partkey
         AND (o.o_orderdate > CAST(v_od AS DATE)
              OR (o.o_orderdate = CAST(v_od AS DATE) AND o.o_orderkey > v_ok))
       ORDER BY o.o_orderdate, o.o_orderkey
       LIMIT 1;
    EXCEPTION
      WHEN NO_DATA_FOUND THEN
        v_ok := NULL;
        v_od := NULL;
    END;
  END LOOP;
END;
/

-- ---------------- 驱动过程：规范基底（FOR-IN + 跳行 trap 内 INSERT）里逐行调用上面的函数 ----------------
-- 调用查询取自各目录 *-pl.sql 文件末尾的 SELECT（:invocations 换成定值，结果写进结果表）。

CREATE OR REPLACE PROCEDURE prc_goto_service IS
BEGIN
  DELETE FROM result_goto_service;
  COMMIT;
  FOR c IN (SELECT c_custkey FROM customer ORDER BY c_custkey LIMIT 300) LOOP
    BEGIN
      INSERT INTO result_goto_service(c_custkey, service_level)
      VALUES (c.c_custkey, service_level(c.c_custkey));
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_goto_ship IS
BEGIN
  DELETE FROM result_goto_ship;
  COMMIT;
  FOR c IN (SELECT c_custkey FROM customer ORDER BY c_custkey LIMIT 300) LOOP
    BEGIN
      INSERT INTO result_goto_ship(c_custkey, shipmode)
      VALUES (c.c_custkey, preferred_shipmode(c.c_custkey));
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_goto_global IS
BEGIN
  DELETE FROM result_goto_global;
  COMMIT;
  FOR o IN (SELECT o_orderkey FROM orders ORDER BY o_orderkey LIMIT 300) LOOP
    BEGIN
      INSERT INTO result_goto_global(l_orderkey, long_distance)
      VALUES (o.o_orderkey, long_distance(o.o_orderkey));
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

-- 原文查询是 supplier × orders 的连接后按 s_name 聚合计数；这里先保留 (s_name, o_orderkey) 粒度的逐行结果，
-- 参照侧再做同样的聚合。
CREATE OR REPLACE PROCEDURE prc_goto_late IS
BEGIN
  DELETE FROM result_goto_late;
  COMMIT;
  FOR o IN (SELECT o_orderkey
              FROM orders
             WHERE o_orderstatus = 'F'
             ORDER BY o_orderkey
             LIMIT 200) LOOP
    FOR s IN (SELECT s.s_suppkey, s.s_name
                FROM supplier s, nation n
               WHERE s.s_nationkey = n.n_nationkey
                 AND n.n_name = 'GERMANY') LOOP
      BEGIN
        INSERT INTO result_goto_late(s_name, o_orderkey, kept_waiting)
        VALUES (s.s_name, o.o_orderkey, order_kept_waiting(s.s_suppkey, o.o_orderkey));
      EXCEPTION
        WHEN OTHERS THEN
          NULL;
      END;
    END LOOP;
  END LOOP;
  COMMIT;
END;
/

-- 驱动调用的是 goto_margin_r（见上）；goto_margin 的忠实版由探针 G.margin.fn 直接调用。
CREATE OR REPLACE PROCEDURE prc_goto_margin IS
  v_buy     NUMBER;
  v_sell    NUMBER;
  v_margin  NUMBER;
BEGIN
  DELETE FROM result_goto_margin;
  COMMIT;
  FOR p IN (SELECT p_partkey FROM part ORDER BY p_partkey LIMIT 100) LOOP
    BEGIN
      goto_margin_r(p.p_partkey, v_buy, v_sell, v_margin);
      INSERT INTO result_goto_margin(p_partkey, buy_orderkey, sell_orderkey, margin)
      VALUES (p.p_partkey, v_buy, v_sell, v_margin);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_goto_savings IS
  v_pct      NUMBER;
  v_changes  NUMBER;
BEGIN
  DELETE FROM result_goto_savings;
  COMMIT;
  FOR o IN (SELECT o_orderkey
              FROM orders
             WHERE o_orderstatus = 'O'
             ORDER BY o_orderkey
             LIMIT 150) LOOP
    BEGIN
      goto_savings(o.o_orderkey, v_pct, v_changes);
      INSERT INTO result_goto_savings(o_orderkey, savings_pct, changes)
      VALUES (o.o_orderkey, v_pct, v_changes);
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

-- prc_goto_late 的「仅改一处」版（prc_goto_late 是忠实版）。唯一改动：内层循环的记录名 s → sup。
-- 原因：内层游标里 supplier 的表别名也叫 s，引擎把这个别名当成了尚未绑定的循环记录变量，
-- 运行期报 ENG-0044 bind record s undefined at runtime（属名字解析缺口，见探针 G.late）。
-- 其余（把内外层记录的字段直接写进 INSERT 与函数实参）与忠实版相同。
CREATE OR REPLACE PROCEDURE prc_goto_late_b IS
BEGIN
  DELETE FROM result_goto_late;
  COMMIT;
  FOR o IN (SELECT o_orderkey
              FROM orders
             WHERE o_orderstatus = 'F'
             ORDER BY o_orderkey
             LIMIT 200) LOOP
    FOR sup IN (SELECT s.s_suppkey, s.s_name
                  FROM supplier s, nation n
                 WHERE s.s_nationkey = n.n_nationkey
                   AND n.n_name = 'GERMANY') LOOP
      BEGIN
        INSERT INTO result_goto_late(s_name, o_orderkey, kept_waiting)
        VALUES (sup.s_name, o.o_orderkey, order_kept_waiting(sup.s_suppkey, o.o_orderkey));
      EXCEPTION
        WHEN OTHERS THEN
          NULL;
      END;
    END LOOP;
  END LOOP;
  COMMIT;
END;
/
