-- 探针 X.dual —— Oracle 伪表 DUAL（非论文案例，是引擎的缺口）。
-- SQL-ProcBench 的 PL/SQL 例程大量使用 `… from dual`（proc31、sudf_8 等）。
-- 引擎只把 sys_dummy 当内建表（verify/deploy.rs 的 ENGINE_PROVIDED_TABLES），DUAL 被当成缺件：
-- 调用时 `MISS relation:dual`，缺件即红。函数体只有一条 SELECT 1 INTO v FROM DUAL。
CREATE OR REPLACE FUNCTION fnc_dual_one RETURN INTEGER IS
  v  INTEGER;
BEGIN
  SELECT 1
    INTO v
    FROM DUAL;
  RETURN v;
END;
/

CREATE OR REPLACE PROCEDURE prc_dual_one IS
BEGIN
  DELETE FROM result_dual;
  COMMIT;
  INSERT INTO result_dual(n)
  VALUES (fnc_dual_one());
  COMMIT;
END;
/
