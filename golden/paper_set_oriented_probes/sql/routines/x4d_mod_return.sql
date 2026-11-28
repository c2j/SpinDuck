-- 探针 X4.retmod —— RETURN (v MOD 3)（v = 7）。
-- 参照是语法错误；2026-11-28 起编译期显式拒绝（BIZ-0126）。单独成文件，理由同 x4c。
CREATE OR REPLACE FUNCTION fnc_num_mod_return RETURN NUMBER IS
  v  NUMBER := 7;
BEGIN
  RETURN (v MOD 3);
END;
/
