CREATE OR REPLACE FUNCTION FNC_GET_SPLIT_DATE (p_i_date_name IN VARCHAR2) RETURN VARCHAR2 IS
  v_date dic_all_kind.kind_name%TYPE;
BEGIN
  SELECT t.kind_name
    INTO v_date
    FROM dic_all_kind t
   WHERE t.operation_kind = 'split_data_flag'
	   AND t.kind_id = p_i_date_name;

  RETURN v_date;
EXCEPTION
  WHEN OTHERS THEN
    RETURN '';
END;
