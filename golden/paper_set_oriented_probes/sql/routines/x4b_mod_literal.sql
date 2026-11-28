-- 探针 X4.lit —— RETURN 7 MOD 3;（字面量上的中缀 MOD，不带括号）。
-- 参照是语法错误；2026-11-28 起编译期显式拒绝（BIZ-0126，词元扫描统一口径，原 ENG-0022）。
-- 与 x4_numeric_division.sql 分开存放：注册失败会连累同一文件的其它函数。
CREATE OR REPLACE FUNCTION fnc_num_mod_literal RETURN NUMBER IS
BEGIN
  RETURN 7 MOD 3;
END;
/
