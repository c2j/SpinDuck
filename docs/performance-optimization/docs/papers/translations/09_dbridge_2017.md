# DBridge: Translating Imperative Code to SQL (SIGMOD 2017)

## 中文标题
DBridge：将命令式代码翻译为SQL

## 核心思想摘要
通过静态控制流/数据流分析（CFG/DFG），将循环自动改写为集合SQL或异步批提交，并提出循环裂变（Loop Fission）将副作用与批量查询解耦。

## 对 SpinDuck 引擎及集合化架构的启示与映射
指导SpinDuck处理带2% A999异常跳行和多表落盘的存储过程时采用Loop Fission（维表查找前置 + Appender批写）。

---

## 论文前两页原文提取预览
```text
DBridge: Translating Imperative Code to SQL
K. Venkatesh Emani Tejas Deshpande
Karthik RamachandrayS. Sudarshan
Indian Institute of Technology, Bombay
{venkateshek, sudarsha}@cse.iitb.ac.in, {tejasdeshpande111, karthik.s.ramachandra}@gmail.com
ABSTRACT
Application programs that access data located remotely (such
as in a database) often perform poorly due to multiple net-
work round trips and transfer of unused data. This situa-
tion is exacerbated in applications that use object-relational
mapping (ORM) frameworks such as Hibernate, as devel-
opers tend to express complex query logic using imperative
code, resulting in poor performance.
DBridge is a system for optimizing data access in database
applications by using static program analysis and program
transformations. Recently, we incorporated a new suite of
optimization techniques into DBridge. These techniques op-
timize database application programs by identifying rela-
tional operations expressed in imperative code, and translat-
ing them into SQL. In this demonstration, we showcase these
techniques using a plugin for the IntelliJ IDEA Java IDE as
the front end. We show the performance gains achieved by
employing our system on real world applications that use
JDBC or Hibernate.
1. INTRODUCTION
Database applications are written using a mix of declara-
tive SQL queries, and imperative code written in languages
such as Java. Developers of database applications express
parts of relational logic in imperative code. In applications
that use object-relational mapping (ORM) frameworks such
as Hibernate, this is a particularly frequent occurrence, since
developers working on objects nd it easier to write com-
plex query logic using imperative code, rather than writing
queries. However, this practice can be inecient because of
multiple network round trips due to iterative invocation of
queries, and transfer of unused data.
As a step towards addressing this issue, in our recent pa-
per [3], we proposed techniques for automatically rewriting
database applications by identifying various relational oper-
ations performed in imperative code, and translating them
Current aliation: Microsoft IDC
yCurrent aliation: Microsoft Gray Systems Lab
Permission to make digital or hard copies of all or part of this work for personal or
classroom use is granted without fee provided that copies are not made or distributed
for proﬁt or commercial advantage and that copies bear this notice and the full cita-
tion on the ﬁrst page. Copyrights for components of this work owned by others than
ACM must be honored. Abstracting with credit is permitted. To copy otherwise, or re-
publish, to post on servers or to redistribute to lists, requires prior speciﬁc permission
and/or a fee. Request permissions from permissions@acm.org.
SIGMOD’17, May 14-19, 2017, Chicago, IL, USA
c2017 ACM. ISBN 978-1-4503-4197-4/17/05. . . $15.00
DOI:http://dx.doi.org/10.1145/3035918.3058747into equivalent SQL queries. Our techniques can detect op
```
