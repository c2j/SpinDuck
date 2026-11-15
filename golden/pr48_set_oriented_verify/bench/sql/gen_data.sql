-- 100k 行确定性 purchase 同形数据（仅在 SpinDuck/DuckDB 侧生成，再经 CSV 同源载入参照侧，
-- 保证两侧源数据逐字一致）。分布模拟真实业务：500 股东代码 × 50 席位 × 1000 证券 × 2000 合约，
-- 买卖方向交替，单一业务日期，成交量取模。
INSERT INTO mid_yjqs_detail_so
SELECT 'G'||lpad((i%500)::VARCHAR,5,'0'),
       'X'||lpad((i%50)::VARCHAR,4,'0'),
       '01',
       'Z'||lpad((i%1000)::VARCHAR,6,'0'),
       '013'||lpad((i%2000)::VARCHAR,9,'0'),
       CASE WHEN i%2=0 THEN 'B' ELSE 'S' END,
       '0',
       '20250715',
       (i%100000)::BIGINT
FROM generate_series(1,100000) t(i);