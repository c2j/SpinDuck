-- 探针 A2 —— Aggify（arXiv:2004.05378）图 2：时间加权累计 ROI。
-- 形态：游标循环做连乘累计 v_acc := v_acc * (1 + roi)；论文明确指出「无法用内置聚合表达」。
-- 出处说明：原文是 Java + JDBC 方法（位图），按 §2.2 正文的公式 (1+r1)(1+r2)…(1+rn) - 1
--   重写为 GaussDB PL/SQL；游标带 ORDER BY m_month（原文按月份顺序迭代）。
CREATE OR REPLACE FUNCTION fnc_cum_roi(p_inv IN VARCHAR2, p_since IN VARCHAR2) RETURN NUMBER IS
  v_roi  NUMBER;
  v_acc  NUMBER := 1;
  CURSOR c_m IS
    SELECT roi
      FROM monthly_investments
     WHERE investor = p_inv
       AND m_month >= p_since
     ORDER BY m_month;
BEGIN
  OPEN c_m;
  LOOP
    FETCH c_m INTO v_roi;
    EXIT WHEN c_m%NOTFOUND;
    v_acc := v_acc * (1 + v_roi);
  END LOOP;
  CLOSE c_m;
  RETURN v_acc - 1;
END;
/

CREATE OR REPLACE PROCEDURE prc_aggify_a2(p_since IN VARCHAR2) IS
BEGIN
  DELETE FROM result_cum_roi;
  COMMIT;
  FOR r IN (SELECT DISTINCT investor FROM monthly_investments ORDER BY investor) LOOP
    INSERT INTO result_cum_roi(investor, cum_roi)
    VALUES (r.investor, fnc_cum_roi(r.investor, p_since));
  END LOOP;
  COMMIT;
END;
/
