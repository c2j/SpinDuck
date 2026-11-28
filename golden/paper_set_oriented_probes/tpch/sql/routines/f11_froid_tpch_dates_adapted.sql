-- 探针 FR.d' —— Froid §11.1 三个日期谓词 UDF 的「仅改一处」版（逐字版见 f11_froid_tpch_dates.sql）。
-- 改动两处：(1) PL 表达式里的 CAST(x AS DATE) 与 日期 + 天数 挪进 SELECT … FROM sys_dummy（由 SQL 侧求值），
--   原因是引擎的 PL 求值器不支持 PL 表达式里的 TypeCast（ENG-0026）；
-- (2) Oracle 的伪表 DUAL 换成 GaussDB 的 sys_dummy，原因是引擎只把 sys_dummy 当内建表，DUAL 会被当成
--   缺件拒执（见探针 X.dual）。函数体其余部分逐字。
-- 驱动过程（prc_froid_q3、prc_prism_hoist）调用的是这一版。

-- §11.1 dbo.isShippedBefore
CREATE OR REPLACE FUNCTION is_shipped_before_r(p_shipdate IN DATE, p_duration IN INTEGER, p_stdatechar IN VARCHAR2) RETURN INTEGER IS
  v_newdate  DATE;
BEGIN
  SELECT CAST(p_stdatechar AS DATE) + p_duration
    INTO v_newdate
    FROM sys_dummy;
  IF p_shipdate > v_newdate THEN
    RETURN 0;
  END IF;
  RETURN 1;
END;
/

-- §11.1 dbo.checkDate
CREATE OR REPLACE FUNCTION check_date_r(p_d IN VARCHAR2, p_odate IN DATE, p_shipdate IN DATE) RETURN INTEGER IS
  v_d  DATE;
BEGIN
  SELECT CAST(p_d AS DATE)
    INTO v_d
    FROM sys_dummy;
  IF p_odate < v_d AND p_shipdate > v_d THEN
    RETURN 1;
  END IF;
  RETURN 0;
END;
/

-- §11.1 dbo.q3conditions
CREATE OR REPLACE FUNCTION q3conditions_r(p_cmkt IN VARCHAR2, p_odate IN DATE, p_shipdate IN DATE) RETURN INTEGER IS
  v_thedate  VARCHAR2(10) := '1995-03-15';
BEGIN
  IF p_cmkt <> 'BUILDING' THEN
    RETURN 0;
  END IF;
  IF check_date_r(v_thedate, p_odate, p_shipdate) = 0 THEN
    RETURN 0;
  END IF;
  IF is_shipped_before_r(p_shipdate, 122, v_thedate) = 0 THEN
    RETURN 0;
  END IF;
  RETURN 1;
END;
/

-- 驱动（规范基底）：逐行调用 q3conditions_r 与 discount_taxprice。
-- 调用查询对应 Froid §11 里 TPC-H Q3 改用 UDF 的谓词：q3conditions(c_mktsegment, o_orderdate, l_shipdate)。
-- 取样条件：订单键是 16 的倍数（跨满整个键范围，覆盖函数里写死的 1995-03-15 阈值两侧），且客户分段为 BUILDING
-- （本工程里客户键 = 1 + (订单键 × 7) mod 1000，分段按客户键 mod 5 轮转，BUILDING 对应余 1）。参照侧用同一条件。
CREATE OR REPLACE PROCEDURE prc_froid_q3 IS
BEGIN
  DELETE FROM result_q3;
  COMMIT;
  FOR r IN (SELECT l.l_orderkey, l.l_linenumber, c.c_mktsegment, o.o_orderdate, l.l_shipdate,
                   l.l_extendedprice, l.l_discount, l.l_tax
              FROM lineitem l, orders o, customer c
             WHERE l.l_orderkey = o.o_orderkey
               AND o.o_custkey = c.c_custkey
               AND l.l_orderkey % 16 = 0
               AND (1 + (l.l_orderkey * 7) % 1000) % 5 = 1
             ORDER BY l.l_orderkey, l.l_linenumber
             LIMIT 500) LOOP
    BEGIN
      INSERT INTO result_q3(l_orderkey, l_linenumber, q3, disc_price)
      VALUES (r.l_orderkey, r.l_linenumber,
              q3conditions_r(r.c_mktsegment, r.o_orderdate, r.l_shipdate),
              discount_taxprice(r.l_extendedprice, r.l_discount, r.l_tax));
    EXCEPTION
      WHEN OTHERS THEN
        NULL;
    END;
  END LOOP;
  COMMIT;
END;
/

-- 探针 PR.* —— PRISM（PVLDB 18(1)）算法 4「Predicate Hoisting」的场景：UDF 出现在查询的 WHERE 里，
-- 论文的做法是把它提升成可做数据跳过的谓词。这里看引擎对「WHERE 里的 UDF」怎么处理（无游标循环）。
CREATE OR REPLACE PROCEDURE prc_prism_hoist IS
BEGIN
  DELETE FROM result_prism_hoist;
  COMMIT;
  INSERT INTO result_prism_hoist(l_orderkey, l_linenumber)
  SELECT l.l_orderkey, l.l_linenumber
    FROM lineitem l
   WHERE l.l_orderkey <= 2000
     AND is_shipped_before_r(l.l_shipdate, 122, '1995-03-15') = 1
   ORDER BY l.l_orderkey, l.l_linenumber;
  COMMIT;
END;
/
