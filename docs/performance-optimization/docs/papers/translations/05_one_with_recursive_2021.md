# One WITH RECURSIVE is Worth Many GOTOs (SIGMOD 2021)

## 中文标题
一个递归公用表表达式（WITH RECURSIVE）胜过无数GOTO

## 核心思想摘要
采用CPS/Trampoline风格将任意WHILE/GOTO/分支控制流展平为单个WITH RECURSIVE递归CTE，无需修改内核即可在主流RDBMS上执行任意控制流。

## 对 SpinDuck 引擎及集合化架构的启示与映射
解决含复杂条件跳转与多出口EXIT WHEN循环的纯SQL表达难题。

---

## 论文前两页原文提取预览
```text
arXiv:2103.05966v2  [math.AP]  26 Jul 2021KINETIC MAXIMAL Lp
µ(Lp)-REGULARITY FOR THE FRACTIONAL
KOLMOGOROV EQUATION WITH VARIABLE DENSITY
LUKAS NIEBEL
Abstract. We consider the Kolmogorov equation, where the right-hand s ide is given by
a non-local integro-diﬀerential operator comparable to th e fractional Laplacian in velocity
with possibly time, space andvelocity dependentdensity. W eprovethat this equation admits
kinetic maximal Lp
µ-regularity under suitable assumptions on the density and o npandµ. We
apply this result to prove short-time existence of strong Lp
µ-solutions to quasilinear non-local
kinetic partial diﬀerential equations.
AMS subject classiﬁcation. 35K59, 35K65, 45K05
Keywords. kineticmaximal Lp-regularity, fractionalKolmogorovequation, non-locali ntegro-
diﬀerential kinetic equations, quasilinear kinetic equati ons
1.Introduction
In Carleman coordinates the Boltzmann equation can be writt en as the sum of a quasilinear
non-local integro-diﬀerential operatorinvelocity andalo wer orderterm[15]. Thenonlinearity
comes from the fact that the density of this non-local operat or operator depends on the
solutionuitself. In particular, this density depends on t,x,v. In recent years the prototype
for the Boltzmann equation, the fractional Kolmogorov equa tion, has been studied in many
directions. In particular Lp-estimates for the fractional Kolmogorov equation were pro ven in
[7] and [5].
In this note we extend these results to study a family of kinet ic partial diﬀerential equations
related to the fractional Kolmogorov equation with variabl e density of the type
(1.1)/braceleftBigg
∂tu+v·∇xu=Am
t,x,vu+Bt,x,vu+cu+f
u(0) =g
witht∈[0,T],x,v∈Rn, whereAm
t,x,vis a non-local integro-diﬀerential operator of order
β∈(0,2) in velocity of the form
[Am
t,x,vu](t,x,v) = p.v./integraldisplay
Rn(u(t,x,v+h)−u(t,x,v))m(t,x,v,h)
|h|n+βdh,
with suitable assumptions made on the density m. The operator Bt,x,vdenotes a lower-order
term acting in xandvandcis assumed to be a bounded function.
We are going to tackle this problem by means of the concept of k inetic maximal Lp
µ-regularity,
recently introduced in [12, 13]. Shortly speaking, kinetic maximalLp
µ(Lq)-regularity, with
Date: July 27, 2021.
The author is supported by a graduate scholarship (”Landesg raduiertenstipendium”) granted by the State
of Baden-Wuerttemberg, Germany (grant number 1902 LGFG-E) .
12 LUKAS NIEBEL
p,q∈(1,∞) andµ∈(1/p,1], characterizes the data fandgin terms of function spaces such
that∂tu+v· ∇xu∈Lp
µ((0,T);Lq(R2n)) andu∈Lp
µ((0,T);Hβ,q
v(R2n)). In particular, this
allows to strictly separate the study of the inhomogeneous p roblem with g= 0 and of the
initial value problem with f= 0. The initial value problem corresponding to (1.1) can be
studied by an analysis of the trace space which has already be en done in [13].
The more involved task is to study Lp
µ(Lp)-estimates for the inhomogeneous problem. Let
us comment on the existing literature concerning such estim ates. Form=m(h) 
```
