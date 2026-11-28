-- 探针 X4 —— PL 表达式里的除法与取余（非论文案例，由探针 G.savings 的参照比对不一致引出）。
-- GaussDB A 模式实测（2026-11-28 openGauss 5.0.0 容器）：/ 永远返回带小数的 NUMBER，
-- 10 / 3 = 3.3333…，1 / 100.0 = 0.01，(1 - 2/3) * 100 = 33.3333…；% 是取余运算符
-- （7 % 3 = 1，符号随被除数）；中缀 MOD 是语法错误（含 MOD( 形态），mod(a,b) 函数形式可用。
-- 除法 4 例已按实测修复；中缀 MOD 4 例自 2026-11-28 起编译期显式拒绝（BIZ-0126）。
CREATE OR REPLACE FUNCTION fnc_num_div_int RETURN NUMBER IS
BEGIN
  RETURN 10 / 3;
END;
/

CREATE OR REPLACE FUNCTION fnc_num_div_expr RETURN NUMBER IS
BEGIN
  RETURN (1 - 2 / 3) * 100;
END;
/

CREATE OR REPLACE FUNCTION fnc_num_div_dec(p_a IN NUMBER, p_b IN NUMBER) RETURN NUMBER IS
BEGIN
  RETURN p_a / p_b;
END;
/

CREATE OR REPLACE FUNCTION fnc_num_div_small RETURN NUMBER IS
  v  NUMBER;
BEGIN
  v := 1 / 100.0;
  RETURN v;
END;
/

-- % 取余运算符（参照实测：7 % 3 = 1）
CREATE OR REPLACE FUNCTION fnc_num_pct RETURN NUMBER IS
  x  NUMBER;
BEGIN
  x := 7 % 3;
  RETURN x;
END;
/

-- 字面量上的中缀 MOD（不带括号）——与其它中缀 MOD 形态一样预期编译失败（BIZ-0126），
-- 因注册失败会连累同文件其它函数，所以单独成文件，见 x4b_mod_literal.sql。
