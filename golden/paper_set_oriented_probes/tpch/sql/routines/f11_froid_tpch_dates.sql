-- 探针 FR.d —— Froid §11.1 的三个带日期转换的谓词 UDF，逐字取自论文（T-SQL → GaussDB PL/SQL）。
-- dateadd(dd, @duration, @d) 写成 d + n（Oracle 的日期加天数），CAST(…) AS DATE 保持在 PL 表达式里。
-- 【预期过不了编译】：引擎的 PL 求值器不支持 PL 表达式里的 TypeCast（ENG-0026），
-- 报错点在 is_shipped_before 的 v_stdate := CAST(p_stdatechar AS DATE)。属引擎缺口，另见 REPORT。
-- 能跑的版本见 f11_froid_tpch_dates_adapted.sql（只把转换挪进 SQL）。

-- §11.1 dbo.isShippedBefore
CREATE OR REPLACE FUNCTION is_shipped_before(p_shipdate IN DATE, p_duration IN INTEGER, p_stdatechar IN VARCHAR2) RETURN INTEGER IS
  v_stdate  DATE;
BEGIN
  v_stdate := CAST(p_stdatechar AS DATE);
  IF p_shipdate > v_stdate + p_duration THEN
    RETURN 0;
  END IF;
  RETURN 1;
END;
/

-- §11.1 dbo.checkDate
CREATE OR REPLACE FUNCTION check_date(p_d IN VARCHAR2, p_odate IN DATE, p_shipdate IN DATE) RETURN INTEGER IS
BEGIN
  IF p_odate < CAST(p_d AS DATE) AND p_shipdate > CAST(p_d AS DATE) THEN
    RETURN 1;
  END IF;
  RETURN 0;
END;
/

-- §11.1 dbo.q3conditions：调用两个 UDF，多个提前返回。
CREATE OR REPLACE FUNCTION q3conditions(p_cmkt IN VARCHAR2, p_odate IN DATE, p_shipdate IN DATE) RETURN INTEGER IS
  v_thedate  VARCHAR2(10) := '1995-03-15';
BEGIN
  IF p_cmkt <> 'BUILDING' THEN
    RETURN 0;
  END IF;
  IF check_date(v_thedate, p_odate, p_shipdate) = 0 THEN
    RETURN 0;
  END IF;
  IF is_shipped_before(p_shipdate, 122, v_thedate) = 0 THEN
    RETURN 0;
  END IF;
  RETURN 1;
END;
/
