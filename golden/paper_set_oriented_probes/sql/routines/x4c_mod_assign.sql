-- 探针 X4.mod —— PL 赋值语句里的中缀取余 x := (v MOD 3)（v = 7）。
-- 参照（openGauss 5.0.0 A 模式实测）是语法错误；2026-11-28 起编译期显式拒绝（BIZ-0126，词元扫描）。
-- 预期注册失败会连累同文件其它函数，所以单独成文件。
CREATE OR REPLACE FUNCTION fnc_num_mod_assign RETURN NUMBER IS
  v  NUMBER := 7;
  x  NUMBER;
BEGIN
  x := (v MOD 3);
  RETURN x;
END;
/
