-- SQL-ProcBench 逐字拷贝（https://github.com/microsoft/SQL-ProcBench ，
--   src/PLSQL，main 分支，2026-11-27 下载；上游文件 sha256 前 16 位 93be90d4dbb8704c）。
-- 仅做两处整理：CRLF→LF；文件尾部的示例调用查询剥离到 cases/procbench_examples/sudf_8_maxReturnClass.sql。
-- 例程定义本身一字未改。
create or replace function maxReturnClass
return char is   --i_class is char(50) data type
    cnt number;
    cls char(50);
    maxClass char(50);
    maxReturn number;
    cursor c1 is  
        select i_class, count(i_class) as cnt
        from catalog_returns, item
        where i_item_sk = cr_item_sk
        group by i_class;
begin
	maxReturn := 0;
    open c1;
    loop
        fetch c1 into cls, cnt;
        exit when c1%NOTFOUND;
        if cnt>maxReturn then
            maxReturn := cnt;
            maxclass := cls;
        end if;
    end loop;
    close c1;
	return maxClass;
end;


