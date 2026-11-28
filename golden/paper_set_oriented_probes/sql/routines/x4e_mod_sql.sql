-- 探针 X4.modsql —— SELECT 7 MOD 3 INTO v FROM sys_dummy。
-- 参照是语法错误；2026-11-28 起编译期显式拒绝（BIZ-0126）。单独成文件，理由同 x4c。
CREATE OR REPLACE FUNCTION fnc_num_mod_sql RETURN NUMBER IS
  v  NUMBER;
BEGIN
  SELECT 7 MOD 3
    INTO v
    FROM sys_dummy;
  RETURN v;
END;
/
