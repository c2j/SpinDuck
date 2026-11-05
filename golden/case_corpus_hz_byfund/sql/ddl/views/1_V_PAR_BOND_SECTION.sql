drop view if exists V_PAR_BOND_SECTION;
CREATE OR REPLACE /*FORCE*/ VIEW V_PAR_BOND_SECTION (SECURITY_ID, SECTION_NUM, SEQ_NO, START_DATE, END_DATE, ACT_CQDATE, PMLL, PRE_PMLL, FACTOR) AS
  SELECT t.security_id,
       t.section_num,
       t.seq_no,
       t.start_date,
       t.end_date,
       t.act_cqdate,
       t.pmll,
       t.pre_pmll,
       t.factor
  FROM par_sys_bond_section t
 union all
SELECT t.security_id,
       t.section_num,
       t.seq_no,
       t.start_date,
       t.end_date,
       t.act_cqdate,
       t.pmll,
       t.pre_pmll,
       t.factor
  FROM par_sys_asst_section t
 union all
SELECT t.security_id,
       t.section_num,
       t.seq_no,
       t.start_date,
       t.end_date,
       t.act_cqdate,
       t.pmll,
       t.pre_pmll,
       1::NUMERIC factor
  FROM par_sys_entrust_section t
 union all
SELECT t.security_id,
       t.section_num,
       t.seq_no,
       t.start_date,
       t.end_date,
       t.act_cqdate,
       t.rate pmll,
       t.rate pre_pmll,
       1::NUMERIC factor
  FROM par_sys_bond_right_section t
 union all
SELECT t.security_id,
       t.section_num,
       t.seq_no,
       t.start_date,
       t.end_date,
       t.act_cqdate,
       t.pmll pmll,
       t.pmll pre_pmll,
       1::NUMERIC factor
  FROM par_sys_financial_product_sect t
  union all
SELECT t.security_id,
       t.section_num,
       t.seq_no,
       t.start_date,
       t.end_date,
       t.act_cqdate,
       t.pmll pmll,
       t.pmll pre_pmll,
       1::NUMERIC factor
  FROM par_sys_annuity_fund_sect t
   union all
  SELECT t.security_id,
       t.section_num,
       t.seq_no,
       t.start_date,
       t.end_date,
       t.act_cqdate,
       t.pmll,
       t.pre_pmll,
       t.factor
  FROM par_sys_assr_section t
 union all
SELECT t.security_id,
       t.section_num,
       t.seq_no,
       t.start_date,
       t.end_date,
       t.act_cqdate,
       t.pmll,
       t.pre_pmll,
       1::NUMERIC factor
  FROM par_sys_debt_loan_section t;
