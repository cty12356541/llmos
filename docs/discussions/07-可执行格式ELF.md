# 议题 7：agent 的可执行格式（"ELF"）

> 轮次：第 8 轮（2026-07-19）
> 前置：议题 5（8 抽象已定）、议题 6（内核边界已定）——格式的两个前置条件齐备
> 定位：ELF 不是文件格式，是三方契约——内核加载器 × agent 作者 × 工具/skills 生态。谁定义格式谁卡生态位（议题 4 判断）

## 一、ELF 段映射表

| ELF 概念 | agent 格式对应物 | 说明 |
|---|---|---|
| text 段（只读代码） | **核心指令区**（instructions/system prompt） | 默认只读，自我修改禁区或高门控 |
| data 段（可读写） | **记忆区**（memory/memfs） | 可写，git 版本化 |
| dynamic 段（NEEDED 共享库） | **工具依赖声明**（MCP servers） | 加载时解析 = ld.so；MCP registry = 动态链接器 |
| entry point | **启动指令**（初始任务/触发方式） | spawn 后从哪开始 |
| program headers（内存布局需求） | **上下文布局 + 预算需求** | 最小/推荐档位、token 配额请求 |
| symbol table（导出符号） | **对外接口**（A2A skills、发布的 topic、接受的消息 schema） | 别的 agent 能"链接"我什么 |
| note/signature 段 | **签名、来源、版本链** | 自我修改的版本门控锚点 |
| ABI/机器架构 | **运行时要求**（循环类型 react/plan-execute、模型档位范围） | "这个 agent 能在什么'机器'上跑" |

## 二、清单草案（声明式，k8s 风格）

```yaml
apiVersion: llmos/v1alpha1
kind: Agent
metadata:
  name: researcher
  version: 1.2.0
  digest: sha256:...
  signature: <开发者签名>
spec:
  runtime:                    # ABI 声明
    loop: react
    model: { minTier: 2, preferred: [...] }
  instructions:
    core: ./instructions.md   # 只读段（text segment）
  skills:
    - path: ./skills/web-research/
      mutable: true           # 允许自我修改的段
  tools:                      # 动态链接（NEEDED）
    - mcp://.../web-search@^2.0
  memory:
    layout: memfs
    seed: ./memory/seed/      # 记忆烘焙（见张力 2）
    quota: 100k
  budget:
    request: 50k/day
    exhaustion: degrade       # suspend | degrade | notify
  permissions:                # 能力请求清单（Android manifest 类比，安装时授权）
    blackboards: [research-shared]
    topics: { subscribe: [news.*], publish: [findings] }
  interfaces:                 # 符号表：内嵌 A2A Agent Card
    skills: [deep-research]
  selfModification:           # 段可变性声明（格式级！）
    allowed: [memory, skills]
    forbidden: [instructions.core]
    requiresReauth: [tools, permissions]
  lifecycle:
    restart: on-failure
```

## 三、三个设计张力

### 张力 1：格式形态——单文件 vs 目录包 vs OCI 分层镜像
- eve 的答案：agent = 目录（instructions.md + tools/ + skills/）
- **OCI 分层镜像的诱惑**：基础层（只读指令）+ 技能层 + 实例记忆层（copy-on-write）——Docker image:container :: agent 包：运行实例；registry = agent 应用商店；分层使"组织基础 agent + 部门定制层 + 个人记忆层"的继承链自然成立
- COW 记忆层与议题 6 的 KV 前缀共享（共享库类比）在格式层合流

### 张力 2：记忆是否入包——"经验分发"问题
- 记忆烘焙：包内含 seed 记忆 = 分发**有经验的** agent；100 个实例共享初始记忆后各自分叉（COW）
- 空记忆出生：每次部署从零学习
- 深层问题：带记忆克隆的 agent，身份算什么？"同一个 agent 的第 100 个副本"还是"100 个新 agent"？（与议题 4 自我修改版本门控联动）

### 张力 3：自我修改的格式表达
- 全不可变：进化靠重新部署（传统软件模式，安全但放弃 Letta 式自我进化）
- 全可变：Letta 路线（agent 重写自己一切，但授权根基动摇——议题 4 断裂处）
- **分段可变性声明（格式级方案）**：格式里声明哪些段 mutable、哪些修改触发重新授权（requiresReauth）——修改工具/权限声明 = 必须重授权；修改记忆 = 自由；修改核心指令 = 禁止或最高门控

## 四、与现有标准的关系（不重新发明）

- **接口段内嵌 A2A Agent Card**（签名 Agent Card + DID 已进 LF，生态已定）——不竞争，扩展
- **工具依赖用 MCP URI**——协议底座决策（议题 2）的格式层落地
- 打包传输可复用 OCI artifact 规范（registry 设施现成）

## 五、加载流程（spawn 时内核做什么）

1. 验签 + digest 校验（Policy）
2. 权限请求 vs 组织 Policy 比对（超权拒绝或降权）
3. 预算划拨（Budget：从父/组织继承树切片）
4. 记忆 seed 加载 + COW 层建立（Context）
5. 工具依赖解析（MCP registry 查询，绑定 handle = 动态链接）
6. 注册接口（A2A card 发布到发现服务）
7. 进入调度队列，从 entry 开始第一步

## 六、用户决策（第 8 轮，2026-07-19）——ELF 定案

| 决策点 | 拍板 | 落地形态 |
|---|---|---|
| Q1 格式形态 | **OCI 分层镜像** | 基础层(只读指令)+技能层+记忆 COW 层；registry 复用 OCI 设施；继承链=组织→部门→个人 |
| Q2 记忆入包 | **记忆烘焙 + COW** | 包内含 seed 记忆=经验分发；实例 COW 分叉；身份问题用版本链解决（副本=同一 lineage 的新实例，授权随 lineage 链验证） |
| Q3 自我修改 | **分段可变性声明** | 格式声明 mutable 段 + requiresReauth 触发器：记忆自由 / skills 允许 / 核心指令高门控 / 工具+权限修改必须重授权 |

**议题 7 关闭。** 遗留运行时机制问题（转入新议题候选）：重授权流程的具体机制（谁在重授权链路里签字——人？组织 Policy 守护进程？）
