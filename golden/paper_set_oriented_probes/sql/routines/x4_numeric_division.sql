-- 探针 X4 —— PL 表达式里的除法与取余（非论文案例，是引擎的数值语义缺口；由探针 G.savings 的参照比对不一致引出）。
-- Oracle 的 NUMBER 语义：/ 永远返回带小数的 NUMBER，10 / 3 = 3.3333…，1 / 100.0 = 0.01，(1 - 2/3) * 100 = 33.3333…；
-- MOD 既是函数也是中缀运算符，7 MOD 3 = 1。探针清单里的期望值按这个语义手写。
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

-- 中缀 MOD 在赋值语句里
CREATE OR REPLACE FUNCTION fnc_num_mod_assign RETURN NUMBER IS
  v  NUMBER := 7;
  x  NUMBER;
BEGIN
  x := (v MOD 3);
  RETURN x;
END;
/

-- 中缀 MOD 在 RETURN 语句里
CREATE OR REPLACE FUNCTION fnc_num_mod_return RETURN NUMBER IS
  v  NUMBER := 7;
BEGIN
  RETURN (v MOD 3);
END;
/

-- 中缀 MOD 经由 SQL 语句
CREATE OR REPLACE FUNCTION fnc_num_mod_sql RETURN NUMBER IS
  v  NUMBER;
BEGIN
  SELECT 7 MOD 3
    INTO v
    FROM sys_dummy;
  RETURN v;
END;
/

-- 字面量上的中缀 MOD（不带括号）——这一条预期连编译都过不了，所以单独成文件，见 x4b_mod_literal.sql。
