-- 种子：三档 stock_kind 优先级（精确 > 组内兜底 'xx00' > 全局 '0000'），由
-- 函数内层 ORDER BY stock_kind DESC + rownum = 1 决定命中
INSERT INTO par_sys_marketrate (market_code, par_code, stock_kind, par_kind, min_tax, inure_begin_date, inure_end_date)
VALUES ('83', 'P1', '0102', 1, 1.5000, '20240101', '20241231');
INSERT INTO par_sys_marketrate (market_code, par_code, stock_kind, par_kind, min_tax, inure_begin_date, inure_end_date)
VALUES ('83', 'P1', '0100', 1, 2.0000, '20240101', '20241231');
INSERT INTO par_sys_marketrate (market_code, par_code, stock_kind, par_kind, min_tax, inure_begin_date, inure_end_date)
VALUES ('83', 'P1', '0000', 1, 3.0000, '20240101', '20241231');
INSERT INTO par_sys_marketrate (market_code, par_code, stock_kind, par_kind, min_tax, inure_begin_date, inure_end_date)
VALUES ('83', 'P2', '0100', 1, 2.5000, '20240101', '20241231');
INSERT INTO par_sys_marketrate (market_code, par_code, stock_kind, par_kind, min_tax, inure_begin_date, inure_end_date)
VALUES ('83', 'P2', '0000', 1, 3.5000, '20240101', '20241231');
INSERT INTO par_sys_marketrate (market_code, par_code, stock_kind, par_kind, min_tax, inure_begin_date, inure_end_date)
VALUES ('83', 'P3', '0000', 1, 4.0000, '20240101', '20241231');
INSERT INTO par_sys_marketrate (market_code, par_code, stock_kind, par_kind, min_tax, inure_begin_date, inure_end_date)
VALUES ('83', 'P1', '0102', 1, 9.9000, '20230101', '20231231');
