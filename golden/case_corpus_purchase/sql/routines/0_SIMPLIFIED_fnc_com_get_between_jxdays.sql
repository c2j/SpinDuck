-- 【简化实现】两个日期之间的计息天数
-- 原实现缺失；与本项目其它计息类简化（prc_com_get_jxdays）保持一致：按自然日差计算，
-- 不区分 bond_accrual_type（实际天数/30天月等）的差异。
CREATE OR REPLACE FUNCTION FNC_COM_GET_BETWEEN_JXDAYS(p_i_begin_date   IN VARCHAR2,
                                                      p_i_end_date     IN VARCHAR2,
                                                      p_i_accrual_type IN VARCHAR2 DEFAULT NULL)
  RETURN NUMBER IS
  RESULT NUMBER := 0;
BEGIN
  IF p_i_begin_date IS NULL OR p_i_end_date IS NULL THEN
    RETURN 0;
  END IF;
  BEGIN
    RESULT := to_date(p_i_end_date, 'yyyymmdd') - to_date(p_i_begin_date, 'yyyymmdd');
  EXCEPTION
    WHEN OTHERS THEN
      RESULT := 0;
  END;
  RETURN nvl(RESULT, 0);
END;
/
