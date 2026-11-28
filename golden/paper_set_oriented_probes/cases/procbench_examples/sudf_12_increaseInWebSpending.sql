-- 取自 SQL-ProcBench sudf_12_increaseInWebSpending.sql 文件尾部：基准自带的示例调用查询（非例程定义）。
--invocation query
select c_customer_sk, increaseInWebSpending(c_customer_sk) 
from customer
where c_customer_sk in 
	(select ws_bill_customer_sk
	from web_sales_history, date_dim
	where d_date_sk = ws_sold_date_sk
		and d_year = 2000

	INTERSECT

	select ws_bill_customer_sk
	from web_sales_history, date_dim
	where d_date_sk = ws_sold_date_sk
		and d_year = 2001
	)
	and increaseInWebSpending(c_customer_sk) > 0;
