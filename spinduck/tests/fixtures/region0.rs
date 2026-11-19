const DDL: &[&str] = &[
    "CREATE TABLE src_cjqs(seq INT, bcrq VARCHAR, zqdm VARCHAR, gddm VARCHAR, \
     xwdm VARCHAR, bs VARCHAR, amt DECIMAL(18,4), tag VARCHAR)",
    "CREATE TABLE dat_fund_cjqs(bcrq VARCHAR, zqdm VARCHAR, gddm VARCHAR, \
     xwdm VARCHAR, bs VARCHAR, amt DECIMAL(18,4), tag VARCHAR, \
     req VARCHAR NOT NULL DEFAULT 'x')",
    "CREATE TABLE dat_fund_xw_venture(bcrq VARCHAR, scdm VARCHAR)",
    "CREATE TABLE qfqscjhbrbb_temp(seq INT, gddm VARCHAR, zqdm VARCHAR, req VARCHAR, \
     split_err_msg VARCHAR, split_flag VARCHAR)",
    "CREATE TABLE lk_partner(k VARCHAR, val VARCHAR)",
    "CREATE TABLE lk_seat(k VARCHAR, val VARCHAR)",
    "CREATE TABLE lk_acct(k VARCHAR, val VARCHAR)",
    "CREATE TABLE fxj_src(seq INT, zqdm VARCHAR, req VARCHAR)",
    "CREATE TABLE fxj_audit(id INT, note VARCHAR)",
    "CREATE TABLE inst_log_log(code VARCHAR, msg VARCHAR)",
    "CREATE TABLE sys_dummy AS SELECT 1 AS x",
];

const SEEDS: &[&str] = &[
    "INSERT INTO src_cjqs VALUES \
     (1,'2024-01-01','Z1','G1','X1','1',100.5,'t1'),\
     (2,'2024-01-01','Z2','G2','X2','-1',200.25,'t2'),\
     (3,'2024-01-01','Z3','G3','X3','1',50.0,'t3')",
    "INSERT INTO qfqscjhbrbb_temp VALUES \
     (1,'G3','Z1',NULL,'INIT','1'),(2,'MISS1','Z1',NULL,'INIT','1'),\
     (3,'G1','Z1','ok','INIT','1'),(4,'G2','Z1','ok','INIT','1'),\
     (5,'G4','Z1','ok4','INIT','1')",
    "INSERT INTO lk_partner VALUES ('G1','FP1'),('G2','FP2'),('G3','FP3'),('G4','FP4')",
    "INSERT INTO lk_seat VALUES ('G2','FS2'),('G3','FS3'),('G4','FS4')",
    "INSERT INTO lk_acct VALUES ('G3','FA3'),('G4','FA4')",
    // 第 1 行 req NULL → c_fxj 内 INSERT 违例 NOT NULL → CLOSE+inst_log+
    // GOTO qfii_start，跳过第 2 行（与 writebuf_policy 的 FXJ 镜像同口径）。
    "INSERT INTO fxj_src VALUES (1,'ZX1',NULL),(2,'ZX2','ok')",
    "INSERT INTO fxj_audit VALUES (1,'pending')",
];

/// inst_log 是真实写日志表的例程（目录摘要 Impure，非 NoOp）。
const INST_LOG: &str = "CREATE OR REPLACE PROCEDURE inst_log(p_code VARCHAR2, p_msg VARCHAR2) IS
  BEGIN
    INSERT INTO inst_log_log(code, msg) VALUES (p_code, p_msg);
  END;";

/// byfund 缩小镜像：区域 0 `c_get_data` 静态游标 + `c_fxj` 动态游标 +
/// QFII 段。INSERT 投影全部来自游标记录字段与形参，不硬编码列清单。
const BYFUND: &str = "CREATE OR REPLACE PROCEDURE prc_trd_hz_byfund_small(\
    p_i_date IN VARCHAR2, p_o_succeed OUT VARCHAR2) IS \
  CURSOR c_get_data IS \
    SELECT seq, bcrq, zqdm, gddm, xwdm, bs, amt, tag FROM src_cjqs WHERE bcrq = p_i_date; \
  r_get_data c_get_data%ROWTYPE; \
  TYPE fxj_cur IS REF CURSOR; \
  c_fxj fxj_cur; \
  v_sql_txt VARCHAR2; v_fxj_zqdm VARCHAR2; v_fxj_req VARCHAR2; \
  v_fund_code VARCHAR2; v_x1 VARCHAR2; v_seat VARCHAR2; v_split_err_msg VARCHAR2; \
  BEGIN \
    p_o_succeed := '0'; \
    DELETE FROM dat_fund_cjqs WHERE bcrq = p_i_date; \
    COMMIT; \
    DELETE FROM dat_fund_xw_venture WHERE bcrq = p_i_date; \
    COMMIT; \
    OPEN c_get_data; \
    LOOP \
      FETCH c_get_data INTO r_get_data; \
      EXIT WHEN c_get_data%NOTFOUND; \
      BEGIN \
        INSERT INTO dat_fund_cjqs (bcrq, zqdm, gddm, xwdm, bs, amt, tag) \
        VALUES (r_get_data.bcrq, r_get_data.zqdm, r_get_data.gddm, r_get_data.xwdm, \
                r_get_data.bs, r_get_data.amt, r_get_data.tag); \
      EXCEPTION WHEN OTHERS THEN \
        inst_log('280', r_get_data.tag); \
        p_o_succeed := 'CJQS_ERR'; \
      END; \
      COMMIT; \
    END LOOP; \
    CLOSE c_get_data; \
    v_sql_txt := 'SELECT zqdm, req FROM fxj_src ORDER BY seq'; \
    BEGIN \
      OPEN c_fxj FOR v_sql_txt; \
    EXCEPTION WHEN OTHERS THEN \
      inst_log('280', 'fxj open failed'); \
      GOTO qfii_start; \
    END; \
    LOOP \
      FETCH c_fxj INTO v_fxj_zqdm, v_fxj_req; \
      EXIT WHEN c_fxj%NOTFOUND; \
      BEGIN \
        INSERT INTO dat_fund_cjqs (bcrq, zqdm, tag, req) \
        VALUES (p_i_date, v_fxj_zqdm, 'FXJ', v_fxj_req); \
        COMMIT; \
      EXCEPTION WHEN OTHERS THEN \
        CLOSE c_fxj; \
        inst_log('280', 'fxj write err ' || v_fxj_zqdm); \
        GOTO qfii_start; \
      END; \
    END LOOP; \
    CLOSE c_fxj; \
    <<qfii_start>> NULL; \
    FOR res IN (SELECT seq, gddm, zqdm, req FROM qfqscjhbrbb_temp ORDER BY seq) LOOP \
      v_split_err_msg := ''; \
      BEGIN \
        SELECT t.val INTO v_fund_code FROM lk_partner t WHERE t.k = res.gddm; \
      EXCEPTION WHEN OTHERS THEN \
        v_split_err_msg := 'E1 partner ' || res.gddm; \
        inst_log('280', v_split_err_msg); \
        UPDATE qfqscjhbrbb_temp SET split_err_msg = v_split_err_msg WHERE seq = res.seq; \
        COMMIT; \
        GOTO next_row; \
      END; \
      BEGIN \
        SELECT t.val INTO v_x1 FROM lk_seat t WHERE t.k = res.gddm; \
      EXCEPTION WHEN OTHERS THEN \
        v_split_err_msg := 'E2 seat ' || res.gddm; \
        inst_log('280', v_split_err_msg); \
        UPDATE qfqscjhbrbb_temp SET split_err_msg = v_split_err_msg WHERE seq = res.seq; \
        COMMIT; \
        GOTO next_row; \
      END; \
      BEGIN \
        SELECT t.val INTO v_seat FROM lk_acct t WHERE t.k = res.gddm; \
      EXCEPTION WHEN OTHERS THEN \
        v_split_err_msg := 'E3 acct ' || res.gddm; \
        inst_log('280', v_split_err_msg); \
        UPDATE qfqscjhbrbb_temp SET split_err_msg = v_split_err_msg WHERE seq = res.seq; \
        COMMIT; \
        GOTO next_row; \
      END; \
      BEGIN \
        INSERT INTO dat_fund_cjqs (bcrq, zqdm, tag, req) \
        VALUES (p_i_date, v_seat, res.gddm, res.req); \
        UPDATE qfqscjhbrbb_temp SET split_err_msg = '成功', split_flag = '0' WHERE seq = res.seq; \
        COMMIT; \
      EXCEPTION WHEN OTHERS THEN \
        inst_log('280', 'cjqs insert failed ' || res.gddm); \
      END; \
      <<next_row>> NULL; \
    END LOOP; \
    UPDATE fxj_audit SET note = 'done' WHERE id = 1; \
    RETURN; END;";

