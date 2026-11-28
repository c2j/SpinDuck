-- 取自 SQL-ProcBench Sudf_20b_GetManufactComplex.sql 文件尾部：基准自带的示例调用查询（非例程定义）。
--Complex Calling Query
select maxsoldItem 
from
(select ss_item_sk as maxSoldItem 
from 
	(select ss_item_sk, sum(cnt) totalCnt
	from
		(select ss_item_sk, count(*) cnt from store_sales_history group by ss_item_sk 
		union all
		select cs_item_sk, count(*) cnt from catalog_sales_history group by cs_item_sk
		union all
		select ws_item_sk, count(*) cnt from web_sales_history group by ws_item_sk )t1
	group by ss_item_sk)t2
order by totalCnt desc
fetch first 25000 rows only
)t3
where getManufact_complex(maxSoldItem) = 'oughtn st';

--Simple Calling Query
select ws_item_sk 
from
	(select  ws_item_sk, count(*) cnt from web_sales group by ws_item_sk order by cnt fetch first 25000 rows only)t1
where getManufact_complex(ws_item_sk) = 'oughtn st';
