-- 【简化实现】两个日期之间的天数（自然日差）
-- 原实现缺失；与本项目其它日历类简化保持一致：按自然日计算，不做节假日/交易日调整。
CREATE OR REPLACE FUNCTION FNC_COM_GET_BETWEEN_DAYS(p_i_begin_date IN VARCHAR2,
                                                    p_i_end_date   IN VARCHAR2)
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
