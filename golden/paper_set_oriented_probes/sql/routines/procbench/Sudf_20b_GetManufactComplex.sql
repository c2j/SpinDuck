-- SQL-ProcBench 逐字拷贝（https://github.com/microsoft/SQL-ProcBench ，
--   src/PLSQL，main 分支，2026-11-27 下载；上游文件 sha256 前 16 位 0e4b2e926846d9ad）。
-- 仅做两处整理：CRLF→LF；文件尾部的示例调用查询剥离到 cases/procbench_examples/Sudf_20b_GetManufactComplex.sql。
-- 例程定义本身一字未改。
create or replace function getManufact_complex(item int)
return char
is
    man char(50);
	cnt1 int; cnt2 int;
begin
	man  := '';  
	-- was this item sold in this year through store or catalog?
	select count(*) into cnt1 from store_sales_history, date_dim where ss_item_sk=item and d_date_sk=ss_sold_date_sk and d_year=2003;
	select count(*) into cnt2 from catalog_sales_history, date_dim where cs_item_sk=item and d_date_sk=cs_sold_date_sk and d_year=2003;
	if(cnt1>0 and cnt2>0) then
		select i_manufact into man from item where i_item_sk = item;
	else
		man := 'outdated item'; --implies that this item is not sold in a recent year at all and is probably outdated
	end if;
    return man;
end;

