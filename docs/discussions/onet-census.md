# O*NET 工作活动普查：八原语覆盖性验证

> 议题 25 证明义务 #1 的执行证据。检验对象：O*NET Content Model 域 4.A「Work Activities」全部 41 项 Generalized Work Activities（GWA）。
> 被检定理（议题 25 §四）：**任何人类组织可分解的审慎型知识工作任务，都可表达为八原语（acquire / generate / verify / relate / transmit / decompose / aggregate / forget）的有限组合。**

## 数据来源

- O*NET OnLine「Browse by Work Activities」官方分类页（含 41 项 GWA 的官方定义）：https://www.onetonline.org/find/descriptor/browse/4.A （站点更新于 2026-05-19，CC-BY 4.0）
- O*NET 30.3 Data Dictionary, Work Activities 文件说明（确认该域恰好 41 个元素）：https://www.onetcenter.org/dictionary/30.3/csv/work_activities.html
- 分类层级：Information Input（5）→ Mental Processes（10）→ Interacting With Others（17）→ Work Output（9），合计 41。

## 一、总览统计

| 指标 | 数量 |
|---|---|
| GWA 总项数 | 41 |
| 界外（纯体力/反应式，定理边界外） | 8 |
| 界内（审慎型知识工作） | 33 |
| 界内·可完整分解 | 30 |
| 界内·候选反例 | 3 |

**结论先行**：覆盖性对界内 33 项中的 30 项直接成立；3 项候选反例共享同一根因——**情感性/关系性劳动的残留成分**（affective residue）不可分解为知识变换原语。该残留与体力活动同为定理边界外成分，建议以「定理陈述增补边界条款」方式修补，而非新增原语（详见 §四）。

## 二、逐项普查表

判定约定：**界内/界外按活动的主要产出物划分**——产出为知识/判断/信息制品者界内；产出为物理状态改变或即时反应式服务者界外。混合活动标注残留成分。

### A. Information Input（5 项，全部界内）

| # | 活动（Element ID） | 简述 | 判定 | 八原语分解 |
|---|---|---|---|---|
| 1 | Getting Information (4.A.1.a.1) | 从一切相关来源观察、接收、获取信息 | 界内 | acquire（采集）→ verify（信源可信度）→ relate（挂接既有知识） |
| 2 | Monitoring Processes, Materials, or Surroundings (4.A.1.a.2) | 监控材料/事件/环境信息以发现或评估问题 | 界内 | acquire（持续采样）→ verify（对基线/阈值比对）→ relate（异常归因）→ transmit（告警上抛） |
| 3 | Identifying Objects, Actions, and Events (4.A.1.b.1) | 通过分类、估算、识别异同、察觉变化来识别信息 | 界内 | acquire → relate（模式匹配归入类目）→ verify（识别置信度） |
| 4 | Inspecting Equipment, Structures, or Materials (4.A.1.b.2) | 检查设备/结构/材料以定位错误或缺陷成因 | 界内 | acquire（传感读数）→ decompose（系统拆件）→ verify（对规约逐项核验）→ relate（缺陷归因）→ generate（诊断结论） |
| 5 | Estimating the Quantifiable Characteristics of Products, Events, or Information (4.A.1.b.3) | 估算尺寸、距离、数量、时间、成本、资源 | 界内 | acquire（参照数据）→ decompose（拆成可估子项）→ generate（分项估计）→ aggregate（合成总量）→ verify（合理性回检） |

### B. Mental Processes（10 项，全部界内，定理核心覆盖区）

| # | 活动（Element ID） | 简述 | 判定 | 八原语分解 |
|---|---|---|---|---|
| 6 | Analyzing Data or Information (4.A.2.a.1) | 拆解信息以识别底层原理、原因或事实 | 界内 | acquire → decompose（拆分为部分）→ relate（归纳原理）→ aggregate（综合）→ verify |
| 7 | Evaluating Information to Determine Compliance with Standards (4.A.2.a.2) | 用相关信息与判断确定事件/过程是否合规 | 界内 | acquire（法规条款 + 证据）→ relate（证据映射条款）→ verify（合规判定）→ generate（审查结论） |
| 8 | Judging the Qualities of Objects, Services, or People (4.A.2.a.3) | 评估事物或人的价值、重要性、质量 | 界内 | acquire（准则 + 观察）→ relate（准则比对）→ aggregate（多维评分合成）→ generate（判断）→ verify |
| 9 | Processing Information (4.A.2.a.4) | 编译、编码、分类、计算、制表、审计、核验信息 | 界内 | acquire → decompose/aggregate（编制与汇总）→ verify（审计核验）→ generate（制品） |
| 10 | Developing Objectives and Strategies (4.A.2.b.1) | 设立长期目标并指定实现策略与行动 | 界内 | acquire（环境扫描）→ generate（候选策略）→ verify（可行性评估）→ decompose（策略展开为行动） |
| 11 | Making Decisions and Solving Problems (4.A.2.b.2) | 分析信息、评估结果以选择最优解 | 界内 | acquire → decompose（问题结构化）→ generate（候选方案）→ verify（方案评估）→ aggregate（收敛抉择）→ transmit（决策下发） |
| 12 | Organizing, Planning, and Prioritizing Work (4.A.2.b.3) | 制定具体目标与计划以排定、组织、完成工作 | 界内 | decompose（目标拆任务）→ relate（依赖关系建边）→ aggregate（优先级排序）→ generate（计划制品） |
| 13 | Scheduling Work and Activities (4.A.2.b.4) | 排程事件、项目、活动及他人工作 | 界内 | acquire（约束采集）→ decompose（工作拆时间槽）→ relate（依赖/冲突）→ aggregate（排程合成）→ verify（冲突消解回检） |
| 14 | Thinking Creatively (4.A.2.b.5) | 开发、设计、创造新应用/想法/关系/系统/产品 | 界内 | acquire（范例与约束）→ relate（跨域类比迁移）→ generate（新构念）→ verify（筛选与检验） |
| 15 | Updating and Using Relevant Knowledge (4.A.2.b.6) | 保持技术更新并在工作中运用新知识 | 界内 | acquire（新知识）→ relate（整合入既有模型）→ **forget（废弃被取代的旧信念）**→ verify（新模型一致性） |

注：第 15 项是 forget 原语必要性最直接的现实对应物——知识更新在语义上包含对失效知识的主动废弃。

### C. Interacting With Others（17 项：界内 15，界外 2；候选反例 3）

| # | 活动（Element ID） | 简述 | 判定 | 八原语分解 / 说明 |
|---|---|---|---|---|
| 16 | Communicating with Supervisors, Peers, or Subordinates (4.A.3.a.1) | 以电话、书面、邮件或面谈向上级/同事/下属提供信息 | 界内 | generate（消息构造）→ transmit（通道投递）；接收侧：acquire → relate |
| 17 | Communicating with People Outside the Organization (4.A.3.a.2) | 与组织外人员沟通、对外代表组织 | 界内 | acquire（对外口径与受众模型）→ generate → transmit → verify（合规与一致性回检） |
| 18 | Interpreting the Meaning of Information for Others (4.A.3.a.3) | 为他人翻译或解释信息含义及用法 | 界内 | acquire → decompose（信息拆解）→ relate（映射到受众心智模型）→ generate（解释制品）→ transmit |
| 19 | Selling or Influencing Others (4.A.3.a.4) | 说服他人购买或改变想法/行动 | 界内（含残留） | acquire（对方立场建模）→ generate（论证）→ transmit → verify（态度改变检测）；**残留：情感联结与魅力成分**（非反例，见 §四） |
| 20 | Resolving Conflicts and Negotiating with Others (4.A.3.a.5) | 处理投诉、平息争端、解决冲突、谈判 | **候选反例 ①** | 认知层可分解：acquire（各方立场）→ decompose（立场 vs 利益）→ relate（共同基础）→ generate（方案）→ transmit → verify（协议达成）；**残留：实时情绪降级与信任修复不可分解**，详析见 §三 |
| 21 | Establishing and Maintaining Interpersonal Relationships (4.A.3.a.6) | 与他人发展并长期维持建设性合作关系 | **候选反例 ②** | 知识层可分解：acquire（对方信息建档）→ relate（关系上下文维护）→ forget（过期印象更新）；**残留：随时间累积的信任/情感本身不是知识变换**，详析见 §三 |
| 22 | Assisting and Caring for Others (4.A.3.a.7) | 向同事/客户/患者提供个人协助、医疗照护、情感支持 | 界外 | 反应式照护与身体照护，产出为身心状态改变；其知识子成分（护理计划）由 #24/#32 覆盖 |
| 23 | Performing for or Working Directly with the Public (4.A.3.a.8) | 为公众表演或直接服务（餐饮/零售/接待） | 界外 | 反应式即时服务；知识密集型服务变体由 #18/#28 覆盖 |
| 24 | Coaching and Developing Others (4.A.3.b.1) | 识别他人发展需求并教练、指导、帮助其提升 | 界内 | acquire（能力评估）→ decompose（技能差距结构化）→ generate（发展计划）→ transmit（反馈/示范）→ verify（进展度量） |
| 25 | Coordinating the Work and Activities of Others (4.A.3.b.2) | 使团队成员协同完成任务 | 界内 | decompose（目标拆派工）→ transmit（任务下发）→ acquire（状态回收）→ verify（进度核验）→ relate（再同步） |
| 26 | Developing and Building Teams (4.A.3.b.3) | 在团队成员间鼓励并建立互信、尊重与合作 | **候选反例 ③** | 知识层可分解：transmit（愿景/规范）→ relate（成员互补性配置）→ verify（协作健康度）；**残留：互信的情感积累不可分解**，详析见 §三 |
| 27 | Guiding, Directing, and Motivating Subordinates (4.A.3.b.4) | 为下属提供指导与方向，设定绩效标准并监督 | 界内（含残留） | decompose（标准设定）→ transmit → acquire（绩效数据）→ verify（对标）→ generate（反馈）；**残留：激励的情感成分** |
| 28 | Providing Consultation and Advice to Others (4.A.3.b.5) | 就技术/系统/流程议题向管理层等提供专家咨询 | 界内 | acquire（客户情境）→ relate（专长映射问题）→ generate（建议）→ transmit → verify（采纳与效果） |
| 29 | Training and Teaching Others (4.A.3.b.6) | 识别教育需求、开发课程、教学 | 界内 | acquire（学情）→ decompose（课程结构化）→ generate（教材）→ transmit（讲授）→ verify（考核） |
| 30 | Monitoring and Controlling Resources (4.A.3.c.1) | 监控并控制资源、监督资金使用 | 界内 | acquire（库存/支出数据）→ verify（对预算核验）→ relate（偏差归因）→ generate（纠正措施）→ transmit |
| 31 | Performing Administrative Activities (4.A.3.c.2) | 日常行政事务：维护信息档案、处理文书 | 界内 | acquire（单据）→ aggregate/decompose（归档分类）→ verify（完整性）→ transmit（提交/流转）→ forget（到期销毁） |
| 32 | Staffing Organizational Units (4.A.3.c.3) | 招聘、面试、选拔、录用、晋升 | 界内 | decompose（岗位需求规格化）→ generate（职位描述）→ acquire（候选人信息）→ verify（筛选核验）→ relate（人岗匹配）→ aggregate（排序录用） |

### D. Work Output（9 项：界内 3，界外 6）

| # | 活动（Element ID） | 简述 | 判定 | 八原语分解 / 说明 |
|---|---|---|---|---|
| 33 | Documenting/Recording Information (4.A.4.a.1) | 以书面或电子形式录入、转录、记录、存储、维护信息 | 界内 | generate/aggregate（记录制品）→ verify（准确性）→ transmit（写入共享记忆/档案）；保留策略到期执行 forget |
| 34 | Drafting, Laying Out, and Specifying Technical Devices, Parts, and Equipment (4.A.4.a.2) | 提供图纸、细则、规格以指导制造/装配/维护 | 界内 | decompose（系统拆件）→ generate（图纸/规格）→ verify（对约束校核）→ transmit（交付下游） |
| 35 | Repairing and Maintaining Electronic Equipment (4.A.4.a.3) | 维修、校准、测试电子设备 | 界外 | 产出为物理状态恢复；其诊断子过程（acquire 症状 → relate 归因 → verify）与 #4 同构，已被覆盖 |
| 36 | Repairing and Maintaining Mechanical Equipment (4.A.4.a.4) | 维修、调试、测试机械设备 | 界外 | 同上 |
| 37 | Working with Computers (4.A.4.a.5) | 使用计算机编程、写软件、设功能、录数据、处理信息 | 界内 | decompose（问题规约化）→ generate（代码/数据处理制品）→ verify（运行测试）→ relate（集成）；注：本项是**元活动**——八原语在数字基板上的执行介质，其覆盖由其余各项的工具化形态自证 |
| 38 | Controlling Machines and Processes (4.A.4.b.1) | 用控制机构或直接体力操作机器/流程 | 界外 | 毫秒—秒级控制环，议题 20/25 已定为界外（反应式控制） |
| 39 | Handling and Moving Objects (4.A.4.b.2) | 用手臂搬运、安装、定位、移动材料 | 界外 | 纯体力 |
| 40 | Operating Vehicles, Mechanized Devices, or Equipment (4.A.4.b.3) | 驾驶车辆或操作机械化设备 | 界外 | 反应式控制 |
| 41 | Performing General Physical Activities (4.A.4.b.4) | 攀爬、举重、平衡、行走等全身性体力活动 | 界外 | 纯体力 |

## 三、候选反例详析

三个候选反例呈现**同一结构性模式**：活动本身 = 可分解的认知层 + 不可分解的情感残留层（affective residue）。

### 反例 ① Resolving Conflicts and Negotiating with Others（#20）

认知层完整可分解（利益拆解、方案生成、协议验证见上表）。但谈判实务中「情绪降级」（de-escalation）的关键操作——语调调节、共情姿态、时机拿捏——不是对信息状态的审慎变换，而是对**他者情感状态**的实时作用。八原语中无任何原语以情感状态为操作对象；且其时间尺度（秒级反应）触边界条款「反应式控制界外」。

### 反例 ② Establishing and Maintaining Interpersonal Relationships（#21）

最强候选。关系的「维持」是跨越数月数年的连续过程，其产出——信任——不满足原语操作对象的类型纪律：acquire/relate/forget 可以维护「关于对方的知识」，但信任本身不是知识制品，无法由 generate 产出、无法由 verify 核验、无法由 transmit 投递。这是唯一一个**其核心产出物整体落在原语类型系统之外**的 GWA。

### 反例 ③ Developing and Building Teams（#26）

与 ② 同根：团队互信是 ② 的关系性产出在群体层面的聚合形态。互补性配置、规范传递均可分解；互信积累不可分解。

### 根因归类

三例可归并为**一类**反例：**情感性/关系性劳动（affective & relational labor）**。其不可分解性与界外体力活动同源——操作对象不是显性知识状态。议题 25 §五已承认隐性知识经 HITL 接地（Polanyi 悖论）；情感残留可视为该边界的对称面：框架处理显性知识变换，情感作用经人接地。

## 四、结论与修补建议

**覆盖性判定：条件成立（成立但需定理陈述增补一条边界条款）。**

1. 41 项 GWA 中，8 项纯体力/反应式活动本在定理界外（与议题 20 既有边界一致，普查未引入新边界）。
2. 界内 33 项中 30 项可完整分解为八原语有限组合，覆盖 Mental Processes 全域（10/10）——即定理的核心主张区无反例。
3. 3 项候选反例归并为单一根因（情感性劳动残留），其认知子层全部可分解。

**修补建议（供设计讨论裁决，二选一）**：

- **方案 A（推荐）· 收窄定理边界**：定理陈述增补：「审慎型知识工作任务**之知识变换成分**可表达为八原语有限组合；情感性/关系性作用成分与体力成分同列界外，经人（HITL）接地。」与既有 Polanyi 边界条款合并为一条「非知识变换残留」条款。成本：仅修订陈述，原语集不动。
- **方案 B · 新增原语**：引入以情感/关系状态为操作对象的第九原语（如 attune）。成本：破坏原语集的类型纪律（操作对象从知识状态扩到情感状态），引理 1–3 需全部重证，且该原语无法由 Agent 自主性实现（触及议题 24 符号主义边界）。不推荐。

**普查对完备性定理的最终裁决：采用方案 A 后，证明义务 #1 关闭。**

## 五、方法学注记

- 判定标准「按产出物类型分界」先于分析确立，避免为迁就定理而事后划界。
- 41 项清单与官方定义逐字取自 onetonline.org 浏览页（2026-05-19 版）；元素计数（41）与 O*NET 30.3 Data Dictionary 交叉核验一致。
- 局限：GWA 是粗粒度分类（约 41 项），其下尚有 ~2,083 项 Detailed Work Activities（DWA）。本普查在 GWA 层面确认覆盖性；若审查者要求 DWA 级证据，可作为证明义务 #1 的后续细化项，但 GWA 级无反例（方案 A 下）已构成对归纳基例的有效压力测试。
