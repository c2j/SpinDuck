-- 论文探针种子数据：全部确定性（无随机、无时间戳），OFF 与集合化两侧共用同一份。
-- 数值刻意覆盖：并列最小值（留给 A1.tie 场景）、空集（part 4 无供应记录 → NULL）、
-- 负收益率、跨账户分组、零计数（getManufact 的两个分支）、SUM 为 NULL 的客户。

-- ---------- TPC-H 子集（A1 / A4 / A6） ----------
INSERT INTO part VALUES (1, 'Part A', 'MFGR1');
INSERT INTO part VALUES (2, 'Part B', 'MFGR1');
INSERT INTO part VALUES (3, 'Part C', 'MFGR2');
INSERT INTO part VALUES (4, 'Part D', 'MFGR2');

INSERT INTO supplier VALUES (1, 'Supplier#000000001', 1000);
INSERT INTO supplier VALUES (2, 'Supplier#000000002', 2000);
INSERT INTO supplier VALUES (3, 'Supplier#000000003', 3000);

INSERT INTO partsupplier VALUES (1, 1, 100.0000, 100);
INSERT INTO partsupplier VALUES (1, 2, 90.0000, 200);
INSERT INTO partsupplier VALUES (2, 1, 50.0000, 300);
INSERT INTO partsupplier VALUES (2, 3, 45.0000, 400);
INSERT INTO partsupplier VALUES (3, 2, 200.0000, 500);

-- ---------- A2：时间加权累计 ROI ----------
INSERT INTO monthly_investments VALUES ('INV1', '202401', 0.010000);
INSERT INTO monthly_investments VALUES ('INV1', '202402', -0.020000);
INSERT INTO monthly_investments VALUES ('INV1', '202403', 0.030000);
INSERT INTO monthly_investments VALUES ('INV2', '202401', 0.050000);
INSERT INTO monthly_investments VALUES ('INV2', '202402', 0.000000);

-- ---------- A3：三个账户的流水 ----------
INSERT INTO txns VALUES (1, 'A', 100.0000);
INSERT INTO txns VALUES (2, 'A', -30.0000);
INSERT INTO txns VALUES (3, 'A', 50.0000);
INSERT INTO txns VALUES (4, 'B', 10.0000);
INSERT INTO txns VALUES (5, 'B', 20.0000);
INSERT INTO txns VALUES (6, 'C', -5.0000);

-- ---------- TPC-DS 子集（C1 / D1 / D5） ----------
INSERT INTO date_dim VALUES (2451114, 2003, 1);
INSERT INTO date_dim VALUES (2451145, 2003, 2);
INSERT INTO date_dim VALUES (2451545, 2023, 1);

INSERT INTO item VALUES (1, 'oughtn st', 'Electronics');
INSERT INTO item VALUES (2, 'oughtn st', 'Jewelry');
INSERT INTO item VALUES (3, 'other mfgr', 'Electronics');
INSERT INTO item VALUES (4, 'oughtn st', 'Sports');

INSERT INTO store_sales_history VALUES (2451114, 1);
INSERT INTO store_sales_history VALUES (2451145, 1);
INSERT INTO store_sales_history VALUES (2451114, 3);

INSERT INTO catalog_sales_history VALUES (2451114, 1);
INSERT INTO catalog_sales_history VALUES (2451145, 1);
INSERT INTO catalog_sales_history VALUES (2451114, 2);

INSERT INTO web_sales VALUES (1, 1001, 60.00);
INSERT INTO web_sales VALUES (1, 1002, 70.00);
INSERT INTO web_sales VALUES (1, 1003, 20.00);
INSERT INTO web_sales VALUES (2, 2001, 80.00);
INSERT INTO web_sales VALUES (2, 2002, 90.00);
INSERT INTO web_sales VALUES (3, 3001, 40.00);
INSERT INTO web_sales VALUES (4, 4001, 25.00);

-- D3（proc62）：退货与原单
INSERT INTO web_returns VALUES (1001, 1, 10.00, 1);
INSERT INTO web_returns VALUES (1002, 2, 5.00, 1);
INSERT INTO web_returns VALUES (1003, 9, 1.00, 1);
INSERT INTO catalog_returns VALUES (2001, 1, 7.00, 1, 2451114);
INSERT INTO catalog_returns VALUES (2002, 2, 3.00, 1, 2451114);
INSERT INTO catalog_sales VALUES (2001, 1, 80.00);
INSERT INTO catalog_sales VALUES (2002, 2, 90.00);
INSERT INTO reason VALUES (1, 'AAAAAA', 'Faulty item');
INSERT INTO reason VALUES (2, 'BBBBBB', 'Wrong item');

-- D1（sudf_8）：i_class 计数 Electronics 3 / Jewelry 1
INSERT INTO catalog_returns VALUES (5001, 1, 1.00, 2, 2451145);
INSERT INTO catalog_returns VALUES (5002, 1, 1.00, 2, 2451145);
INSERT INTO catalog_returns VALUES (5003, 2, 1.00, 2, 2451145);
INSERT INTO catalog_returns VALUES (5004, 3, 1.00, 2, 2451145);

-- D5（sudf_12）：客户 1/2 两年都有消费，客户 3 只有 2000 年
INSERT INTO web_sales_history VALUES (2451114, 1, 50.00);
INSERT INTO web_sales_history VALUES (2451145, 1, 80.00);
INSERT INTO web_sales_history VALUES (2451114, 2, 40.00);
INSERT INTO web_sales_history VALUES (2451145, 2, 60.00);
INSERT INTO web_sales_history VALUES (2451114, 3, 30.00);

-- D2'：促销 2 不在「最佳促销」清单里，应保持不变
INSERT INTO promotion VALUES (1, 2451114, 'N');
INSERT INTO promotion VALUES (2, 2451114, 'N');
INSERT INTO promotion VALUES (3, 2451114, 'N');
INSERT INTO best_promos_catalog VALUES (1);
INSERT INTO best_promos_catalog VALUES (3);

-- ---------- B1：Froid total_price ----------
INSERT INTO customer VALUES (1, 1, 'Customer#000000001');
INSERT INTO customer VALUES (2, 2, 'Customer#000000002');
INSERT INTO customer VALUES (3, 3, 'Customer#000000003');
-- Froid 图 1：xchg_rate(@default_currency, @pref_currency) 换算方向是「默认货币 → 偏好货币」
INSERT INTO customer_prefs VALUES (1, 'USD');
INSERT INTO customer_prefs VALUES (2, 'EUR');
INSERT INTO customer_prefs VALUES (3, 'JPY');
INSERT INTO orders VALUES (1, 1, 100.50);
INSERT INTO orders VALUES (2, 1, 200.00);
INSERT INTO orders VALUES (3, 2, 300.25);
INSERT INTO xchg VALUES ('USD', 'EUR', 0.900000);
INSERT INTO xchg VALUES ('USD', 'JPY', 150.000000);

-- ---------- E1：walk() 的 MDP。动作 a 在 80% 的情况下按规定走，20% 偏到 s_to 之外的邻态；本数据里每个动作只有一个转移。 ----------
INSERT INTO states VALUES (1, 'start', 1.00);
INSERT INTO states VALUES (2, 'mid', 2.00);
INSERT INTO states VALUES (3, 'goal', 10.00);

INSERT INTO policy VALUES (1, 'a');
INSERT INTO policy VALUES (2, 'a');
INSERT INTO policy VALUES (3, 'a');

INSERT INTO actions VALUES (1, 'a', 1, 2, 1.0000);
INSERT INTO actions VALUES (2, 'a', 2, 3, 1.0000);
INSERT INTO actions VALUES (3, 'a', 3, 3, 1.0000);

-- (start_state, success_at, failure_at, max_steps)：第一组走满 5 步；第二组第 1 步就越过上界；第三组一步不走
INSERT INTO walk_params VALUES (1, 100, -100, 5);
INSERT INTO walk_params VALUES (1, 1, -1, 10);
INSERT INTO walk_params VALUES (3, 100, -100, 0);

-- ---------- D09：DBridge 2017 图 1 ----------
INSERT INTO project VALUES (1, 0, 'alpha');
INSERT INTO project VALUES (2, 1, 'beta');
INSERT INTO project VALUES (3, 0, 'gamma');
INSERT INTO project VALUES (4, 1, 'delta');

-- ---------- Q10：QBS 2013 图 1（用户 3 没有角色） ----------
INSERT INTO users VALUES (1, 10, 'u1');
INSERT INTO users VALUES (2, 20, 'u2');
INSERT INTO users VALUES (3, NULL, 'u3');
INSERT INTO users VALUES (4, 10, 'u4');
INSERT INTO roles VALUES (10, 'admin');
INSERT INTO roles VALUES (20, 'dev');
INSERT INTO roles VALUES (30, 'ops');

-- ---------- S11：Sloth 2014 图 1（患者 3 没有任何记录） ----------
INSERT INTO patient VALUES (1, 'p1');
INSERT INTO patient VALUES (2, 'p2');
INSERT INTO patient VALUES (3, 'p3');
INSERT INTO encounter VALUES (101, 1);
INSERT INTO encounter VALUES (102, 1);
INSERT INTO encounter VALUES (201, 2);
INSERT INTO visit VALUES (1001, 1, 1);
INSERT INTO visit VALUES (1002, 1, 0);
INSERT INTO visit VALUES (1003, 1, 1);
INSERT INTO visit VALUES (2001, 2, 0);

-- ---------- T2.f：仓库夹具 caller_dummy 的数据（取自 set_oriented_dummy.rs 的 ddl1()） ----------
INSERT INTO seat_para VALUES ('S1', '11', '1', 0.0500, 0.0200);
INSERT INTO seat_para VALUES ('S2', '12', '1', 0.1000, 0.0100);
INSERT INTO seat_para VALUES ('S3', '99', '0', 0.5000, 0.5000);
INSERT INTO cur_rows VALUES ('S1', 1000.00, 111.11, 222.22, 'a');
INSERT INTO cur_rows VALUES ('S2', 500.50, 11.11, 22.22, 'b');
INSERT INTO cur_rows VALUES ('S3', 77.77, 7.77, 8.88, 'c');
INSERT INTO cur_rows VALUES ('S4', 9.99, 1.11, 2.22, 'd');
