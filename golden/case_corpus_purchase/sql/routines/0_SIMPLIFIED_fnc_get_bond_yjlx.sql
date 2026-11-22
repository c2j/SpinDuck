-- 【简化实现】债券应计利息（fnc_get_bond_yjlx）
-- 退役语料 case_corpus_hz_byfund 的逐字实现把 decode/round/nvl/例程调用
-- 深层嵌套在单个 SELECT 表达式里，表达式深度超过 ogsql-parser 的递归
-- 预算（实测登记需 >2MiB 栈），无法在本仓库例程目录登记；该调用位于
-- prc_trd_hz_byfund 后缀段（c_get_data 区域 0 之后），不影响集合化
-- 区域 0 的证明目标。此处按 pmll × 自然日数 / 365 近似，语义有损、
-- 仅供 Off/BatchAtomic 一致性对比使用。
CREATE OR REPLACE FUNCTION FNC_GET_BOND_YJLX(p_i_security_id VARCHAR2,
                                             p_i_date        VARCHAR2,
                                             p_i_flag        VARCHAR2 DEFAULT '1',
                                             p_i_change_date VARCHAR2 DEFAULT '0',
                                             p_i_is_qdii     VARCHAR2 DEFAULT '0',
                                             p_i_fund_code   VARCHAR2 DEFAULT '')
  RETURN NUMBER IS
  v_pmll      NUMBER := 0;
  v_begin     VARCHAR2(8) := '';
  v_days      NUMBER := 0;
BEGIN
  BEGIN
    SELECT NVL(t.pmll, 0), t.inure_begin_date
      INTO v_pmll, v_begin
      FROM v_par_bond t
     WHERE t.security_id = p_i_security_id
       AND p_i_date BETWEEN t.inure_begin_date AND t.inure_end_date;
  EXCEPTION
    WHEN OTHERS THEN
      v_pmll := 0;
      v_begin := '';
  END;
  v_days := FNC_COM_GET_BETWEEN_DAYS(v_begin, p_i_date);
  RETURN NVL(v_pmll, 0) * NVL(v_days, 0) / 365;
END;
/
