-- 探针 B1 —— Froid（arXiv:1712.00498，PVLDB 2018）图 1：total_price / xchg_rate，逐字取自论文。
-- 原文 T-SQL，改写成 GaussDB PL/SQL，只有语法层面的改动：
--   [头]   头部语法；declare … = 初值 → 声明段；
--   [赋]   select @v = … → SELECT … INTO；select @rate = xchg_rate(…) → PL 赋值；
--          return (select rate from dbo.xchg …) 里的标量子查询 → SELECT INTO 再返回；
--   [串]   str(@price) → TO_CHAR(v_price)。
-- 表名与列名沿用论文：customer、orders(o_custkey, o_totalprice)、customer_prefs(custkey, currency)、xchg(from_cur, to_cur, rate)。
-- 语义提示（不是引擎问题）：客户没有订单时 @price 为 NULL，T-SQL 里 str(NULL) + 'EUR' 得 NULL，
-- 而 Oracle/GaussDB 把 NULL 当空串，拼接结果是 'EUR'。本包的种子不区分这两种语义。

-- Figure 1：create function xchg_rate(@from char(3), @to char(3)) returns float
CREATE OR REPLACE FUNCTION xchg_rate(p_from IN CHAR, p_to IN CHAR) RETURN NUMBER IS
  v_rate  NUMBER;
BEGIN
  SELECT rate
    INTO v_rate
    FROM xchg
   WHERE from_cur = p_from
     AND to_cur = p_to;
  RETURN v_rate;
END;
/

-- Figure 1：create function total_price(@key int) returns char(50)
CREATE OR REPLACE FUNCTION total_price(p_key IN INTEGER) RETURN VARCHAR2 IS
  v_price            NUMBER;
  v_rate             NUMBER;
  v_pref_currency    CHAR(3);
  v_default_currency CHAR(3) := 'USD';
BEGIN
  SELECT SUM(o_totalprice)
    INTO v_price
    FROM orders
   WHERE o_custkey = p_key;
  SELECT currency
    INTO v_pref_currency
    FROM customer_prefs
   WHERE custkey = p_key;
  IF v_pref_currency <> v_default_currency THEN
    v_rate := xchg_rate(v_default_currency, v_pref_currency);
    v_price := v_price * v_rate;
  END IF;
  RETURN TO_CHAR(v_price) || v_pref_currency;
END;
/

-- 论文 §2.1 的调用查询：select c_name, dbo.total_price(c_custkey) from customer;
-- 同一条查询见 probes.tsv 的 B1.q（走 query 通道，直接在 SQL 里调 UDF）。
-- prc_froid_b1s 是把它放进例程里的形态（单条 INSERT … SELECT，无游标循环）。
CREATE OR REPLACE PROCEDURE prc_froid_b1s IS
BEGIN
  DELETE FROM result_total;
  COMMIT;
  INSERT INTO result_total(c_custkey, c_name, total_txt)
  SELECT c_custkey, c_name, total_price(c_custkey)
    FROM customer
   ORDER BY c_custkey;
  COMMIT;
END;
/

CREATE OR REPLACE PROCEDURE prc_froid_b1 IS
BEGIN
  DELETE FROM result_total;
  COMMIT;
  FOR r IN (SELECT c_custkey, c_name FROM customer ORDER BY c_custkey) LOOP
    INSERT INTO result_total(c_custkey, c_name, total_txt)
    VALUES (r.c_custkey, r.c_name, total_price(r.c_custkey));
  END LOOP;
  COMMIT;
END;
/
