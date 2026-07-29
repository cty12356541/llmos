# NLOS 站点设计系统（v10 · 连续画布与渐进式密度）

> 本文件是 docs/site/ 的视觉契约。所有 token 来自 2026-07-23 对
> https://www.langchain.com（首页、/langgraph、/langsmith/observability）的
> Playwright 实机截取（1440px 整页截图 + getComputedStyle 提取），不是凭印象的描述。

## v10 连续画布与信息密度修订（2026-07-30）

- 首页首屏只保留 NLOS 原生标志与“自然表达 / 系统执行 / 全局可控”三组设计哲学词；
- 标志中心字标必须完整显示 `NLOS`；`NL` 只能作为历史缩写，不再作为主 Logo；
- 首屏 Logo 与哲学词同时进入 GSAP lead targets，并有独立 CSS 可见性兜底，动效失效不得产生空首页；
- 项目定义、执行层级、实现状态和设计论证全部下沉到后续章节与专题页；
- 章节不再依靠整块背景切换分隔，改用共享深色画布、弱光场和纵向系统轴；
- 章节标题正文限制为约 68 字符行宽，卡片正文限制为约 62 字符行宽；
- 桌面端高密度四列统计降为两列，首页阅读入口降为两列，卡片间距和章节留白统一增加；
- 六个专题页共享“愿景 → 系统边界 → 执行 → 验证 → 资源 → 实现”探索路径；
- 相邻章节显示语义交接轴；Timeline、抽象、系统调用、预算、决策、Tier 与高密度 Grid 的次要项均进入可逆详细层；
- 普通论证不再默认使用四边封闭玻璃卡：双栏、原则、决策和补充说明改用开放上边线、序号与留白；封闭玻璃只表示系统对象、边界或交互节点；
- 文字入场以 7–10px 轻位移、1.25px 弱模糊、0.82s `power3.out` 为基线，禁止所有字体统一使用强模糊、缩放和短促上浮；
- 主 Logo 内部为延伸十字光轴，交点使用极亮星点；完整 `NLOS` 字标位于星点下方，避免遮挡交点。
- v10.3 移除 Logo 外圆与十字端点；无限标记使用放大的数学符号 `∞`，不使用带圆环的 Unicode `♾︎`，其中心与十字、星点共用同一坐标；
- v10.5 无限符号改由 Canvas 参数函数绘制：Gerono 双纽线 `x=A·cos(t), y=B·sin(2t)`，左右严格对称，中心交点天然与十字和星点同坐标；
- Canvas 先绘制低亮底线和柔光，再沿参数 `t` 绘制 62 段渐亮彗尾；完整周期约 4.8 秒，reduced-motion 固定为静态高光，并保留数学 `∞` 字形作为无 Canvas 兜底；
- v10.5.1 同时绘制第二条彗尾，逐段使用 `P(π-t)=2C-P(t)` 做中心反演，保证两条彗尾的位置、尾长、亮度与运动严格中心对称；
- v10.5.2 将 `NLOS` 确立为首屏最大品牌文字（3.4–5.4rem），三组核心哲学词退居第二层（1.7–3rem），英文全称仅作最小辅助说明；十字横轴扩展到容器 172%，纵轴收束到 112%，形成约 1.54:1 的主从比例；
- v10.6 首页历史论证由四列伪表格改为开放式历史轨道：五个阶段全部默认展开，左侧节点表达时间顺序，中部光路表达“成熟计算范式 → NLOS 机制”，右侧路线状态独立对齐；桌面端使用稳定语义列，移动端转换为单向纵读；
- v10.6 删除全站内容密度折叠机制：Card、Budget、Abstraction、Syscall、Tier、Stat、Timeline 等正文集合始终完整显示，不再生成“展开/收起详细层”按钮；仅保留移动端主导航和系统图焦点等具有独立操作语义的交互；
- 静态 Card、Stat、Timeline、Tier、Budget、Abstraction 与 Syscall Group 取消四边封闭面，改为开放基线、列间隔和留白；
- Callout 改为页边批注，Chip 与页内导航改为文字索引；封闭边界仅保留给架构层、图节点、代码区和真正可操作控件；
- 桌面端节标题采用“页边索引 / 主命题阅读列”的非对称编辑网格，避免所有信息继续堆叠在中央卡片墙内。
- 每屏遵循“一项主结论 + 一项主视觉关系 + 少量注释”，避免标题、长段落、图和卡片墙争夺同一视觉焦点。

## v9 深色界面连续性修订（2026-07-30）

- 原 `.section.light` 只作为内容语义类保留，不再切换为白色画布；
- vision 边界、execution 规模、roadmap 技术栈三个章节统一为深色高透玻璃；
- 浅色章节内的 Card、Stat、Callout、Timeline 全部回归同一冰蓝描边与玻璃层级；
- 禁止在深色页面流程中插入大面积纯白章节或纯白信息卡，白色仅用于高对比文字和极小高光。

## v8 内容图谱与透明度修订（2026-07-29）

- 子窗口 tint 从约 `0.05–0.07` 降至 `0.026–0.042`，blur 从 `20–22px` 降至 `15–16px`，让背景极光和细线穿过内容面板；
- 高透不等于低可读性：文字仍使用 `--white/--text-2`，边缘继续保留镜面高光和渐变描边；
- 设计理念优先转译为纯 HTML/CSS 系统图，不使用装饰性 AI 插画：
  - 完整 NLOS 系统分层图；
  - Global→Cell→Worker 海量 Agent 调度图；
  - 双 TaskAttempt→TaskHead CAS 唯一提交图；
  - Resource Manager 多资源控制图；
- 图形语言固定为：冰蓝 1px 连接线、透明节点、等宽层级标签、中心权威节点微光、移动端单列退化；
- 图必须表达对象、边界、数据流或状态迁移，不能只把段落放进更大的卡片。

---

## 0. 实地研究证据

- 截图与原始 token 数据：临时目录 `langchain-research/`（home/langgraph/langsmith 整页 + hero 截图 + tokens.json）
- 提取方法：Chromium 实机渲染，`getComputedStyle` 全站普查（颜色/背景/圆角/边框/字体出现频次排序）
- 网络说明：首页因 `ajax.googleapis.com`（Webflow webfont loader）在无梯环境下超时，导致 networkidle 永不触发；改用 `waitUntil: 'commit'` 后完整加载，三页全部拿到真实渲染结果

## 1. LangChain 的真实设计语言（实测，非印象）

### 1.1 一句话总结

**极暗藏青底 + 单一冰蓝 + 细线流场艺术 + 轻字重大标题 + 等宽字体做标签层，深/浅区块交替。**
克制、工程感、单色纪律。全站几乎找不到一处装饰性渐变。

### 1.2 颜色 token（getComputedStyle 实测值）

| 用途 | 实测值 | hex |
|---|---|---|
| 页面底色 | `rgb(3, 7, 16)` | `#030710` |
| 区块带（band） | `rgb(13, 19, 34)` | `#0D1322` |
| 卡片/容器面 | `rgb(22, 31, 52)` 及其透明变体 | `#161F34` |
| 浅色区块底 | `rgb(242, 250, 255)` | `#F2FAFF` |
| 大标题（深色区） | `rgb(127, 200, 255)` | `#7FC8FF` |
| 正文（深色区） | `rgb(204, 233, 255)` | `#CCE9FF` |
| 次级正文 | `rgba(255,255,255,.6)` / `rgba(3,7,16,.6)` | — |
| 主按钮 | 底 `rgb(229,244,255)` `#E5F4FF`，字 `rgb(3,7,16)` | — |
| 次按钮 | 透明底，`1px solid rgb(47,75,104)` `#2F4B68` | — |
| 边框 | `1px solid #2F4B68`（可见）/ `#161F34`（隐性）/ `2px solid #1E3C5A` | — |
| 浅底链接/强调 | `rgb(0, 109, 221)` `#006DDD` | — |

**颜色普查结论**：深色区文本颜色前三名是 `#7FC8FF`（33–45 次）、`#FFFFFF`、`#CCE9FF`。
全站只有蓝—白—藏青一个色相家族。没有 teal、没有紫、没有琥珀色。

### 1.3 渐变普查（关键证据）

全页扫描 `background-image: *gradient*` 的元素，只发现 **2 处**，且都是**遮罩渐变**
（`linear-gradient(90deg, #030710 35%, transparent)`，用于让插画边缘溶入底色）。
**零装饰渐变、零渐变文字、零紫蓝渐变。** 上一版的三色渐变文字在 LangChain 语言里不存在。

### 1.4 字体与排版（实测）

- 正文：`Aeonik, Tahoma, sans-serif`，16px / 24px（行高 1.5）
- 展示标题：`Twklausanne`，**字重 300**（不是粗体！），64px，字距 -1.92px（-0.03em），行高 1.1
- 导航链接、按钮、eyebrow 标签、箭头链接、产品名：**等宽字体**（截图可辨），这是全站的"工程纹理"
- 大标题颜色 = 纯色 `#7FC8FF`，仅首页 hero 标题带一层很淡的光晕（text-shadow 级，不是 glow 特效）

### 1.5 形状与边界

- 圆角普查：**6px 占绝对主导**（17 次），其次 50%（圆形）、30px（pill）、8px。
  没有 16px 大圆角卡片——上一版的 `radius-l: 16px` 方向错误
- 边框即分层：深色区几乎不用阴影，层级全靠 `#161F34` / `#2F4B68` 两档 1px 边框
- 按钮：主 = 冰白底黑字 6px；次 = 透明 + `#2F4B68` 边 6px；`padding: 12px 18px`，14px

### 1.6 布局节奏

- 容器 `max-width: 1416px`，导航 `1456px`，导航高 72px
- 区块底色按 **深 → 深 → 带（#0D1322）→ 浅（#F2FAFF，连续 1–2 个）→ 深（CTA/页脚）** 交替，
  浅色区是节奏里的"换气口"，不是可选项
- 内容主模式：**左文右图交替行**（标题 + 灰文 + 等宽箭头链接 / 深色视觉卡），不是均等卡片网格
- 统计数据：超大数字（64px+）+ 等宽小标签

### 1.7 签名视觉元素：细线流场（line-art flow）

LangChain 真正的识别物不是渐变，而是**细线流场**：1px 冰蓝/钢蓝曲线组成的
喷泉/波浪/汇聚形态——首页 hero 下方 "Build / Test / Deploy / Monitor" 四颗 pill
挂在一根横线上，曲线向下汇聚成一股；/langgraph hero 右侧大尺度流线；资源卡缩略图；
页脚 CTA。它精确、数学、工程化，与"发光网格"是完全相反的审美。

### 1.8 页脚

深色，链接分栏，底部**巨型描边 "LangChain" 字标**（outline text）收束全页。

---

## 2. 上一版（v1）的"AI 味"病灶清单

| # | 病灶（v1 实际代码） | 为什么是 AI 套路 | 对应改进（v2 已执行） |
|---|---|---|---|
| 1 | `--grad: teal→blue→purple` 三色渐变文字用于 h2 `<em>`、hero 句号、tier 序号、principle 序号（`background-clip: text` 共 6 处） | "AI 生成页"的第一指纹；实测 LangChain 渐变数为 0 | 全部改纯色 `#7FC8FF`；标题内强调用白色；渐变变量删除 |
| 2 | `.bg-glow` 三团 teal/blue/purple 径向光晕铺满视口 | 无信息量的"氛围光"，模板标配 | 删除；深色区底色纯净 `#030710` |
| 3 | `.bg-grid` 全页 56px 细网格 + 顶部 mask | "科技感网格"是第二大 AI 指纹；LangChain 无网格 | 删除；换成细线流场 SVG（hero 汇聚喷泉、页脚） |
| 4 | 抽象卡图标四色轮换（teal/blue/purple/amber 彩虹芯片） | 装饰性多彩 = 模板感；LangChain 单色纪律 | 全部冰蓝单色，语义靠文字不靠色相 |
| 5 | 语义色泛滥：`--amber` `--purple` `--red` `--blue` 各管一摊 | 色相被当成免费的分层手段 | 收敛为 冰蓝 / 白 / 藏青灰 三档 + 唯一例外：尸检章用一枚克制的红 |
| 6 | 卡片全部 `radius: 16px` + 均等网格铺满每个区块 | 千篇一律的"SaaS 卡片墙"；实测 LangChain 主圆角 6px | 圆角系统改 6/8px；区块版式多样化：表格行 / 左右交替 / 堆叠层 / 大数字，不再每节都是同构卡片网格 |
| 7 | hero-thesis 盒子带 3px 渐变左边条 + 泛光 callout（`--grad-soft` 底） | 装饰条无信息 | 论点改等宽一行式排版；callout 改 1px 边框净面板 |
| 8 | tier-bar 渐变进度条 | 渐变当装饰 | 纯色细条 |
| 9 | 导航 logo 渐变方块 + 渐变 favicon | 同上 | 纯色冰白方块 + 藏青字 |
| 10 | 视觉重心平铺：每节都是 eyebrow+h2+卡片网格，无深浅节奏、无超大字体对比 | 无节奏 = 无设计 | 引入 LangChain 节奏：深→深→带→浅→深；hero 64px/300 字重大标题；Budget/随机性区用大数字；页脚巨型描边字标 |
| 11 | h2 用 `font-weight: 750` 粗黑 | 与 LangChain 的 300 轻字重大标题方向相反；粗黑大标题是 AI 页另一指纹 | 展示标题全部 300 字重 + -0.03em 字距 |
| 12 | hero 以产品名 "NLOS." 作主标题 | 模板式 brand-hero；hero 标题应是价值主张 | hero 主标题改为价值主张（价值句），产品名退到导航/页脚 |

## 3. v2 设计 token（落地值）

```css
--bg:        #030710;   /* 页面底 */
--bg-band:   #0D1322;   /* 区块带 */
--surface:   #0A1220;   /* 卡片面（实测 #161F34 的暗化变体，保持卡片轻于 band） */
--line:      #161F34;   /* 隐性边框 */
--line-2:    #2F4B68;   /* 可见边框 */
--ink:       #7FC8FF;   /* 标题冰蓝 */
--text:      #CCE9FF;   /* 正文 */
--text-2:    rgba(204,233,255,.62);
--text-3:    rgba(204,233,255,.42);
--white:     #FFFFFF;
--ice:       #E5F4FF;   /* 主按钮底 */
--light-bg:  #F2FAFF;   /* 浅区块底 */
--light-text:#030710;
--light-2:   rgba(3,7,16,.62);
--light-line:rgba(3,7,16,.14);
--link-light:#006DDD;
--danger:    #E5636C;   /* 唯一例外色：尸检章 */

--radius-s: 6px; --radius-m: 8px; --radius-pill: 999px;
--font-display: 300 字重系统无衬线栈（Twklausanne 的离线替代）
--font-mono: ui-monospace 栈（标签层）
```

排版：hero 64px/300/-0.03em/1.1（clamp 38–64px）；h2 32–40px/300；
正文 16px/1.6；eyebrow 等宽 12px/大写/宽字距。
区块节奏：hero(深) → 历史(深) → 问题(深) → 架构(band) → 抽象(深) →
系统调用(浅) → ELF(浅) → Budget(深) → 随机性(深) → 原则(深) → 页脚(深)。

动效：仅 IntersectionObserver 渐入（600ms 内、translateY ≤16px），
reduced-motion 全量兜底，无 JS 全可见。签名元素：细线流场 SVG（1px 曲线，无动画装饰）。

### 3.1 液态玻璃 token（v7 修订）

v7 把全部「经典毛玻璃」（深色调 tint `rgba(13,22,38,0.55–0.72)` + blur）替换为
**液态玻璃**：低不透明度冰白 tint，让深暗极光背景真正透过来。单色纪律不变
（只允许冰蓝 / 白 / 藏青家族），圆角 / 布局 / 字号体系不动，只改表面材质。

```css
--glass-bg: linear-gradient(135deg, rgba(255,255,255,.06), rgba(255,255,255,.02) 42%, transparent 60%),
  rgba(214,238,255,.045);       /* 玻璃面：斜向折射微光 + 冰白 tint（α ≤ 0.07） */
--glass-bg-strong: 同上结构，tint α 0.06（glacier 层 0.05 / 0.07）
--glass-blur: 20px;             /* glacier 层 22px；≤720px 降档 14px 减轻 GPU 负担 */
--glass-saturate: 2.0;          /* 高饱和让背景光透出来（彩光不是灰光） */
--glass-border: rgba(255,255,255,.22);        /* 描边底色（渐变环 fallback，glacier .24） */
--glass-border-hover: rgba(255,255,255,.48);  /* glacier .50 */
--glass-highlight: rgba(255,255,255,.40);     /* 顶部镜面高光（inset 0 1px 0，glacier .44） */
--glass-edge-top: rgba(255,255,255,.45);      /* 渐变描边：上缘亮（glacier .50） */
--glass-edge-bottom: rgba(127,200,255,.10);   /* 渐变描边：下缘暗冰蓝（glacier .11） */
--glass-edge-top-hover / --glass-edge-bottom-hover: 0.62 / 0.20（glacier 0.66 / 0.21，hover 同步调亮）
--shadow-glass: …, inset 0 1px 0 var(--glass-highlight);   /* 镜面高光并入阴影 token */
```

**三条规则：**

1. **tint ≤ 0.07**：玻璃面一律冰白 / 白色 tint（α 0.03–0.07），禁止回到深色
   navy tint（0.55+）——深色 tint 会闷死背景极光，液态感来自「透」。
2. **blur 14–22px + saturate(2.0)**：所有 `backdrop-filter` 统一走
   `blur(var(--glass-blur)) saturate(var(--glass-saturate))`；更高模糊让
   背景更均匀柔和，低 tint 下文字反而更清晰；单独调 blur 不加 saturate
   等于只做了一半（透过来的是灰光不是彩光）。移动端
   （≤720px）`--glass-blur` 降档至 14px 减轻 GPU 负担，高光/描边不变。
3. **镜面高光边**：每个玻璃面必须有 (a) 顶部 `inset 0 1px 0` 镜面高光
   （α 0.40–0.44，已并入 `--shadow-glass`）+ (b) 1px 渐变描边环（上
   `--glass-edge-top` → 下 `--glass-edge-bottom`，::before + mask-composite
   技法，`pointer-events: none`）+ (c) 斜向 135° 折射微光（已并入
   `--glass-bg`）。hover 时高光与描边同步调亮，过渡 ≤ 220ms。

**浅区变体**：`.section.light` 玻璃用白色 tint（α 0.55）+ 深色 hairline
（`--light-line`），描边环改为白高光 → `rgba(3,7,16,.06)`，文字 #030710 系可读。

**兜底**：`@supports not (backdrop-filter: blur(1px))` 时 `--glass-bg*` 回退为
α 0.94+ 的纯色藏青面；`prefers-reduced-transparency: reduce` 时玻璃面不透明化
并移除全部 backdrop-filter。两种情况下文字对比度不低于无玻璃状态。
