-- 探针 FR.* —— Froid（arXiv:1712.00498，PVLDB 2018）第 11 节的 TPC-H 谓词 UDF，逐字取自论文。
-- 这几个 UDF 同时是 PRISM（PVLDB 18(1)）与 Batching（CIDR 2024）实验用的负载。
-- 原文是 T-SQL，改写成 GaussDB PL/SQL，只有三类语法改动：
--   [声明] T-SQL 的 declare … = 初值 → GaussDB 的声明段前置（常量用字面量代替）；
--   [返回] IF (…) RETURN n; → IF … THEN RETURN n; END IF;
--   [调用] dbo.fn(…) → fn(…)。
-- 函数体逻辑逐字保留。含日期转换的三个函数见 f11_froid_tpch_dates*.sql（逐字版与「仅改一处」版）。

-- §11.1 dbo.discount_price：单 RETURN 纯表达式。
CREATE OR REPLACE FUNCTION discount_price(p_extprice IN NUMBER, p_disc IN NUMBER) RETURN NUMBER IS
BEGIN
  RETURN p_extprice * (1 - p_disc);
END;
/

-- §11.1 dbo.discount_taxprice：嵌套调用另一个 UDF。
CREATE OR REPLACE FUNCTION discount_taxprice(p_extprice IN NUMBER, p_disc IN NUMBER, p_tax IN NUMBER) RETURN NUMBER IS
BEGIN
  RETURN discount_price(p_extprice, p_disc) * (1 + p_tax);
END;
/
