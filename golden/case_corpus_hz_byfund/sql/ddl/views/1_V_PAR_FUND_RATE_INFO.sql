CREATE OR REPLACE /*FORCE*/ VIEW V_PAR_FUND_RATE_INFO (FUND_CODE, OPER_TYPE, RATE_FLAG, INURE_BEGIN_DATE, INURE_END_DATE, OPERATOR, STOCK_KIND, TAX_RATE, MARKET_CODE) AS 
  SELECT s.fund_code,
       oper_type,
       rate_flag,
       inure_begin_date,
       inure_end_date,
       operator,
       stock_kind,
       tax_rate,
       t.market_code
  FROM par_fund_rate_info t, par_drv_config s
 WHERE s.event_code = '26'
   AND s.event_type = '0'
   AND t.fund_code = s.fund_code_source
   AND NOT EXISTS (SELECT 1
          FROM par_fund_rate_info g
         WHERE g.fund_code = s.fund_code
           AND t.oper_type = g.oper_type
           AND t.stock_kind = g.stock_kind)
UNION ALL
SELECT FUND_CODE,OPER_TYPE,RATE_FLAG,INURE_BEGIN_DATE,INURE_END_DATE,OPERATOR,STOCK_KIND,TAX_RATE,MARKET_CODE FROM par_fund_rate_info;
