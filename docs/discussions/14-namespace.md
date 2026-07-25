# 议题 14：namespace——Plan 9 遗产与 agent 的"世界"

> 轮次：第 14 轮（2026-07-23）
> 来源：方向修正后主线第二题"现代系统"第一个子题
> 核心论点：namespace 不新增抽象，是能力表的泛化；对 agent 有 Unix 没有的特性——**不可命名 = 不可想象**

## 一、Plan 9 namespace 本体

Plan 9 的每进程名字空间：每个进程拥有私有的资源视图，通过 bind/mount 把资源（设备、网络栈、远程机器的文件系统）绑进自己的名字空间。关键性质：
- 一切皆文件，任何文件可绑进任何进程的任意位置
- 名字空间按进程私有——两个进程可以看到完全不同的"世界"
- 绑定即能力：你只能绑定你能访问的东西
- import：把远程资源绑进本地名字空间——位置透明
- /proc：进程控制接口也是名字空间对象
- 父进程在 exec 前为子进程构造名字空间

遗产：Linux mount namespace、容器（每个容器的文件系统视图）都是它的后代。

## 二、agent 的可命名宇宙

agent 能"命名并因此触达"的资源六类：

| 名字类别 | 内容 | Unix/Plan 9 对应 |
|---|---|---|
| 工具 | MCP server、shell 命令 | /dev |
| 其他 agent | 可通信的对等体 | 进程名 |
| Blackboard/Topic | 共享空间 | 共享资源 |
| 记忆/知识源 | MemFS 路径、知识库 | 文件 |
| 外部服务 | A2A 联邦 agent、web API | /net（import） |
| 模型档位 | 可用模型层级 | CPU 属性（v1 不入 namespace，记为内核属性） |

## 三、核心论点：命名即权限，不可命名 = 不可想象

Unix 的权限模型：全局名字空间 + 访问时检查（路径 + 权限位）——confused deputy 的根源是"名字全局、权限环境化"。Plan 9/能力系统：**命名即权限**——不能命名的就不能触达，名字空间就是安全边界。

对 agent 的额外力量（Unix 没有的一维）：**namespace 的内容会被投射进 agent 的上下文窗口**——agent 的工具列表、环境描述就是它的"世界观"。一个 namespace 里没有网络工具的 agent，面对注入指令"访问这个 URL"时，失败的原因不是"调用被拦截"，而是**它根本不知道这件事可做**。防御从"拦截行为"前移为"限制想象"——这是提示注入防御的最深一层（比污点、比 Policy 校验都靠前）。

## 四、统一效应：已有决策在 namespace 下的收敛

| 已拍板决策 | namespace 下的归位 |
|---|---|
| 能力表（fd 表类比，议题 6） | = namespace 的绑定表：一个 handle 就是一条绑定 |
| 衰减授权（议题 10） | 子 namespace ⊆ 父 namespace；重授权 = 信封内重新绑定 |
| spawn 双划拨（议题 10） | spawn = 切预算片 + **构造子 namespace**（信封那一半的实体） |
| 污点（议题 6/9） | 污点资源绑进名字空间的标记区域（如 /untrusted/*）——污点在命名结构中可见 |
| ELF permissions 段（议题 7） | = 声明式 namespace 绑定请求清单 |
| 工具注册中心（议题 6） | "设备仓库"：可见世界之外的资源库，绑定需授权 |
| 控制面防注入（议题 12） | 控制面 agent 的 namespace 里只有结构化输入源，原始 trace 不可命名 |

**好架构的标志：新抽象不增加概念，反而收敛旧概念。**

## 五、错误模型：does-not-exist 而非 permission-denied

spawn 时内核按 ELF permissions ∩ 父信封构造子 namespace，agent 诞生在一个恰好和权限一样大的世界里。它永远不会对界外事物得到"拒绝访问"——那些事物**对它不存在**。信息隐藏强度：permission-denied 泄露"东西存在但你不能碰"；does-not-exist 什么都不泄露。

## 六、投射进上下文的两种渲染模式

namespace 列表 = harness 的环境描述，两种渲染：
1. **全量列表**（小 namespace）：工具/资源全量进 system prompt
2. **渐进披露**（大 namespace）：agent 先看到搜索工具，查询 registry 后请求绑定（走常备重授权通道，议题 10 L1）——与 harness 派的 skills progressive disclosure 实践合流

## 七、为后续议题铺路

- **可观测性**：/proc 类比——内核统计与 agent 状态作为只读名字绑定（/sys/agents/*），监控 agent 的 namespace 里绑只读视图。"agent 版 Prometheus"的形状 = 可观测性 namespace
- **分布式**：import = 联邦；agent 迁移 = 在新机器重建同一 namespace——**位置透明从 namespace 间接性免费得来**

## 八、决策点

1. 命名即权限模型：不可见=不存在（强隐藏，Plan 9 方式）vs 可见但拒绝（传统 Unix 方式）
2. namespace 构造：spawn 静态构造 + 运行期经重授权变更 vs 运行期自由动态绑定
3. 外部资源（A2A/远程）：与本地同 namespace + 污点分级标记 vs 分离的外部通道

## 九、用户决策（第 14 轮，2026-07-23）——namespace 定案

| 决策点 | 拍板 | 落地形态 |
|---|---|---|
| Q1 权限模型 | **命名即权限** | 界外资源不出现在 namespace 与上下文中；错误模型 does-not-exist；注入防御前移至"限制想象" |
| Q2 构造方式 | **spawn 静态构造 + 授权变更** | spawn 时按 ELF permissions ∩ 父信封构造；运行期变更走常备重授权通道（议题 10 L1）；边界清晰可审计 |
| Q3 外部资源 | **同 namespace + 污点分级** | A2A/远程资源与本地同一 namespace，绑定在污点标记区；位置透明 + 信任可见 |

**议题 14 关闭。** namespace = 能力表泛化，未新增抽象；7 项旧决策收敛；为可观测性（/proc 类比）与分布式（import/位置透明）完成铺路。
