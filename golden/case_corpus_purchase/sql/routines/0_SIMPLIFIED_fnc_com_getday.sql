CREATE OR REPLACE FUNCTION fnc_com_getday(p_i_scdm VARCHAR2, p_i_date VARCHAR2, p_i_offset NUMBER)
RETURN VARCHAR2 IS
BEGIN
  RETURN to_char(to_date(p_i_date, 'yyyymmdd') + p_i_offset, 'yyyymmdd');
END;
