-- SQL-ProcBench 逐字拷贝（https://github.com/microsoft/SQL-ProcBench ，
--   src/PLSQL，main 分支，2026-11-27 下载；上游文件 sha256 前 16 位 dd11b6cbf90e2136）。
-- 仅做两处整理：CRLF→LF；文件尾部的示例调用查询剥离到 cases/procbench_examples/sudf_12_increaseInWebSpending.sql。
-- 例程定义本身一字未改。
--Of the customers who purchased from web in both years 2000 and 2001, find positive increase in spending from one year to the other.

create or replace function increaseInWebSpending(cust_sk int)
return number
is
    spending1  number;
	spending2  number;
	increase  number;
begin
	spending1:=0;
    spending2:=0;
    increase:=0;
    
	select sum(ws_net_paid_inc_ship_tax) into spending1 
    from web_sales_history, date_dim
    where d_date_sk = ws_sold_date_sk
        and d_year = 2001
        and ws_bill_customer_sk=cust_sk;
        
	select sum(ws_net_paid_inc_ship_tax)  into spending2
    from web_sales_history, date_dim
    where d_date_sk = ws_sold_date_sk
        and d_year = 2000
        and ws_bill_customer_sk=cust_sk;
        
	if(spending1<spending2) then
		return -1;
	else
		increase := spending1-spending2;
    end if;
	return increase;

end;

