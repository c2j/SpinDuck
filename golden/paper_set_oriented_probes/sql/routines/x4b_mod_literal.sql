-- 探针 X4.lit —— RETURN 7 MOD 3;（字面量上的中缀 MOD，不带括号）。
-- 与 x4_numeric_division.sql 分开存放：这一条预期连编译都过不了（ENG-0022），放在同一个文件会连累其它函数注册。
CREATE OR REPLACE FUNCTION fnc_num_mod_literal RETURN NUMBER IS
BEGIN
  RETURN 7 MOD 3;
END;
/
