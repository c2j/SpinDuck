-- 探针 T2.f —— 仓库自己的可转换夹具 caller_dummy，逐字取自
--   spinduck/tests/set_oriented_dummy.rs 的 `fn caller()`（只去掉 Rust 字符串续行的反斜杠）。
-- 形态：byfund c_get_data 段同型——四次带 WHEN OTHERS 兜底的维表查找 +
--   sys_dummy 上的 CASE 折叠赋值 + IF … THEN 数值覆写 + trap 内 INSERT。
-- 用途：它是测试里断言为 Converted 的复杂形状；在本探针包里必须同样 Converted，
--   否则说明探针环境与测试口径不一致，矩阵里的 Skipped 不可解读。
CREATE OR REPLACE PROCEDURE caller_dummy(p_o_succeed OUT VARCHAR2) IS
       v_commision_type VARCHAR2(4);
       v_return_rate_flag VARCHAR2(4);
       v_return_rate_org NUMBER;
       v_return_rate NUMBER;
       v_yj NUMBER;
     BEGIN
       p_o_succeed := '0';
       FOR r IN SELECT security_id, cjje, yj11, yj12, tag FROM cur_rows LOOP
         BEGIN
           SELECT commision_type INTO v_commision_type
             FROM seat_para WHERE security_id = r.security_id;
         EXCEPTION WHEN OTHERS THEN v_commision_type := '00'; END;
         BEGIN
           SELECT return_rate_flag INTO v_return_rate_flag
             FROM seat_para WHERE security_id = r.security_id;
         EXCEPTION WHEN OTHERS THEN v_return_rate_flag := '0'; END;
         BEGIN
           SELECT return_rate_org INTO v_return_rate_org
             FROM seat_para WHERE security_id = r.security_id;
         EXCEPTION WHEN OTHERS THEN v_return_rate_org := 0; END;
         BEGIN
           SELECT return_rate INTO v_return_rate
             FROM seat_para WHERE security_id = r.security_id;
         EXCEPTION WHEN OTHERS THEN v_return_rate := 0; END;
         BEGIN
           SELECT CASE v_commision_type
                    WHEN '11' THEN r.yj11
                    WHEN '12' THEN r.yj12
                    ELSE 0
                  END
             INTO v_yj
             FROM sys_dummy;
         EXCEPTION WHEN OTHERS THEN v_yj := 0; END;
         IF v_return_rate_flag = '1' THEN
           v_yj := r.cjje * v_return_rate_org - r.cjje * v_return_rate;
         END IF;
         INSERT INTO dst_rows(security_id, yj, tag)
           VALUES (r.security_id, v_yj, r.tag);
       END LOOP;
     END;
/
