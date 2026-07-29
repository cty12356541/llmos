# 面向 llmos/NLOS 的 Rust 核心特性导读

> 目标：不是把 Rust 语法讲全，而是帮助不熟悉 Rust 的项目负责人理解为什么阶段 B 选择 Rust、代码在表达什么，以及哪些能力仍需要架构和测试保证。

## 1. Rust 的定位

Rust 是无垃圾回收器的系统编程语言。它的核心思路是：

> 把内存所有权、共享方式、跨线程安全、错误分支和状态穷尽尽可能提前到编译期检查。

它适合 NLOS 的原因不是“性能高”这么简单，而是 NLOS 本身充满所有权问题：

- Capability 由谁持有和委托；
- ResourceAllocation 何时释放；
- Process/Fiber 是否仍是当前 generation；
- Message 能否跨线程；
- 状态机是否遗漏某个 failure state；
- Operation 取消后 callback 是否还能写回；
- handle 是否在关闭后继续使用。

Rust 能帮我们缩小这些错误空间，但不能自动证明分布式一致性、权限策略、durability 或语义正确性。

## 2. Ownership：每个值有明确所有者

```rust
fn consume_permit(permit: EffectPermit) {
    // permit 被移动到这里
}

let permit = issue_permit();
consume_permit(permit);
// 再使用 permit 会编译失败
```

默认情况下，复杂值赋给另一个变量或传入函数会发生 `move`，不是隐式复制。

对 NLOS 的价值：

- 一次性 permit/token 可以设计成消费后不可再次使用；
- connection、file、lock、allocation 离开作用域时自动清理；
- 避免同一个 mutable state 被多个模块随意持有。

局限：

- 如果类型被设计成 `Clone`，仍可能复制；
- durable 或跨进程的一次性语义不能只靠 ownership，仍需数据库 CAS/fence；
- `Arc`、interior mutability 和 `unsafe` 可以扩大共享，必须约束。

## 3. Borrowing：借用而不转移所有权

```rust
fn inspect_task(task: &Task) {
    // 只读借用
}

fn update_task(task: &mut Task) {
    // 独占可变借用
}
```

核心规则可以粗略理解为：

```text
同一时刻：
多个只读借用
或
一个可变借用
```

这类似内存内的轻量读写隔离。编译器能阻止大量 use-after-free、悬垂指针和无锁数据竞争。

它不是数据库事务，也不跨进程。TaskHead 的并发更新仍需 revision/CAS。

## 4. Lifetime：引用不能活得比对象更久

Lifetime 描述引用的有效关系。多数时候编译器自动推导：

```rust
fn task_name(task: &Task) -> &str {
    &task.name
}
```

它能阻止函数返回指向已销毁局部变量的引用。NLOS 中它适合约束：

- request-local view；
- borrowed capability view；
- transaction/session 生命周期；
- runtime callback 捕获的引用。

稳定 Handle/ID 通常不应该建模成 Rust reference；跨 await、跨进程或持久化对象必须使用独立 ID + generation。

## 5. Struct 与名义类型：避免 ID 混用

```rust
struct ProcessId(Uuid);
struct AgentInstanceId(Uuid);
struct ExecutionFiberId(Uuid);
```

即使内部都是 UUID，它们也是不同类型：

```rust
fn kill_process(id: ProcessId) {}

let fiber_id = ExecutionFiberId::new();
// kill_process(fiber_id); // 编译失败
```

这直接服务于 v0.5 的 nominal ID 要求，避免把所有 ID 降成可互换字符串。

## 6. Enum 与 match：显式状态机

Rust 的 `enum` 是带数据的 sum type：

```rust
enum OperationState {
    Pending,
    Running { started_at: Instant },
    Completed { receipt: ReceiptId },
    CancelledBeforeEffect,
    PartialEffect { receipt: ReceiptId },
    EffectUnknown { operation: OperationId },
}
```

`match` 默认要求覆盖全部分支：

```rust
match state {
    OperationState::Pending => {}
    OperationState::Running { .. } => {}
    OperationState::Completed { receipt } => {}
    OperationState::CancelledBeforeEffect => {}
    OperationState::PartialEffect { receipt } => {}
    OperationState::EffectUnknown { operation } => {}
}
```

以后增加新状态，相关 `match` 可能直接编译失败，提醒实现者处理新语义。这非常适合 Task、Process、Reservation 和 Operation 状态机。

## 7. Option 与 Result：空值和错误进入类型

Rust 不鼓励到处使用 null：

```rust
Option<ReceiptId> = Some(id) | None
```

可恢复错误使用：

```rust
Result<T, E> = Ok(value) | Err(error)
```

```rust
fn commit_task(command: CommitCommand) -> Result<Receipt, CommitError> {
    // `?` 会在出错时提前返回
    validate(&command)?;
    persist(&command)?;
    Ok(build_receipt(command))
}
```

Rust 官方将可恢复错误主要建模为 `Result<T, E>`，不可恢复程序缺陷才使用 panic。[Rust Error Handling](https://doc.rust-lang.org/stable/book/ch09-00-error-handling.html)

NLOS 稳定边界应使用 typed error；普通输入、provider failure、permission denial 不得用 panic。

## 8. Trait：定义能力和可替换边界

Trait 类似“行为契约”，但不等于远程协议：

```rust
trait RuntimeAdapter {
    fn spawn_fiber(&self, spec: FiberSpec)
        -> Result<FiberHandle, RuntimeError>;
}
```

可有多种实现：

```text
TokioRuntimeAdapter
DeterministicTestRuntime
FutureAlternativeRuntime
```

阶段 B 用 trait 隔离 Tokio、SQLite、Wasmtime 和平台 API：

- `RuntimeAdapter`
- `AuthorityStore`
- `ProcessSupervisor`
- `ResourceController`
- `Driver`

Trait 是 Rust 进程内接口；KABI/SABI 仍由 Protobuf/WIT/CBOR 等稳定 schema 定义。

## 9. Generics 与零成本抽象

```rust
fn verify_id<T: StableId>(id: &T) { /* ... */ }
```

Generics 允许复用算法，同时保留类型。编译器通常会为具体类型生成专门代码，不需要传统运行时反射开销。

项目中应避免过度泛型化：公开系统语义宁可使用清晰具体类型，复杂泛型主要留在内部库。

## 10. RAII 与 Drop：作用域结束自动清理

锁、文件、临时目录等对象离开作用域会运行 `Drop`：

```rust
{
    let guard = mutex.lock()?;
    // 使用受保护状态
} // guard 自动释放锁
```

它适合本机临时资源清理，但不能把 `Drop` 当 durable protocol：

- 程序 `kill -9` 时不会运行 Drop；
- 外部副作用可能无法撤销；
- Reservation/Lease 必须依赖持久状态和 reconciliation。

## 11. Send 与 Sync：跨线程安全进入类型系统

- `Send`：值可以安全移动到另一个线程；
- `Sync`：共享引用可以安全被多个线程使用。

Tokio 多线程 runtime 通常要求 Future 满足 `Send`。这能阻止部分线程不安全对象被错误跨线程使用。

它只证明 Rust 内存模型下的线程安全，不证明业务级线性一致性或无死锁。

## 12. async/await 与 Future

```rust
async fn invoke_driver(
    operation: OperationId,
) -> Result<Receipt, DriverError> {
    let response = driver_call(operation).await?;
    reconcile(response).await
}
```

`async fn` 返回一个 `Future`。Future 只有被 executor poll 时才推进；遇到 `.await` 可以挂起，把宿主线程还给其他任务。

这使有限线程承载大量 waiting Fiber 成为可能：

```text
100K waiting Fiber
不等于
100K host thread
```

重要限制：

- async 不自动提供结构化取消；
- drop Future 不等于外部副作用停止；
- CPU 密集代码不 await 时仍会阻塞 worker；
- Tokio task 不是 NLOS ExecutionFiber identity；
- OperationId、cancel epoch、generation fence 必须由 NLOS runtime 注入。

## 13. Arc、Mutex、RwLock 与 Channel

- `Arc<T>`：线程安全引用计数共享所有权；
- `Mutex<T>`：互斥访问；
- `RwLock<T>`：多读或单写；
- channel：消息传递。

项目政策：

- 优先显式 ownership 和消息传递；
- 所有 channel 必须有界；
- 不把整个系统状态放进一个 `Arc<Mutex<_>>`；
- lock 不跨 `.await`，除非经过明确审计；
- authority state 以数据库 transaction/CAS 为准，不以内存锁为最终权威。

## 14. Unsafe

`unsafe` 允许执行编译器无法证明安全的操作，例如：

- 调用 C/系统 API；
- 解引用裸指针；
- 构建底层数据结构。

`unsafe` 不是“关闭所有检查”，而是实现者承诺维护一组额外不变量。

阶段 B 政策：

- 默认业务 crate 禁止 unsafe；
- FFI/平台 adapter 集中使用；
- 每个 unsafe block 写明 Safety invariant；
- 测试 Miri/sanitizer/fuzz；
- unsafe 代码变化需要单独评审。

## 15. Cargo、Crate 与 Workspace

- `cargo`：构建、依赖、测试和发布工具；
- crate：Rust 编译/包单位；
- workspace：管理多个相关 crate；
- `Cargo.toml`：package/workspace 配置；
- `Cargo.lock`：解析后的精确依赖版本。

llmos 采用多 crate workspace：

```text
nlos-types       最底层名义类型
nlos-runtime     runtime contract
nlos-store       authority storage
nlos-supervisor  Process/IsolationUnit
nlos-resource    resource controllers
nlos-cli         SystemControl client
```

依赖方向必须单向；`nlos-types` 不依赖 Tokio、SQLite、Wasmtime 或 UI。

## 16. Rust 工具链

- `rustc`：编译器；
- `cargo`：构建和包管理；
- `rustfmt`：统一格式；
- `clippy`：额外错误/风格检查；
- `rust-analyzer`：编辑器语言服务；
- `rustup`：安装、升级和切换 toolchain。

Rust 官方推荐使用 rustup 管理工具链；default profile 包含 rustc、Cargo、rustfmt、Clippy 和本地文档。[rustup 安装](https://rust-lang.github.io/rustup/installation/)、[rustup components](https://rust-lang.github.io/rustup/concepts/components.html)

项目使用 `rust-toolchain.toml` 声明 stable 和组件，使开发机与 CI 更一致。

## 17. 阅读代码时的快速翻译

| Rust 写法 | 可以先理解为 |
|---|---|
| `T` | 拥有一个 T |
| `&T` | 临时只读借用 |
| `&mut T` | 临时独占可变借用 |
| `Arc<T>` | 跨线程共享所有权 |
| `Option<T>` | 有或没有 |
| `Result<T, E>` | 成功值或类型化错误 |
| `enum` | 有限状态/带数据的联合类型 |
| `trait` | 可替换行为边界 |
| `impl` | 类型或 trait 的实现 |
| `async fn` | 可挂起、返回 Future 的函数 |
| `.await` | 当前 Future 等待并允许 executor 调度其他工作 |
| `?` | 错误则提前返回 |
| `move` | 转移所有权/把捕获值移入 closure |
| `where` | 泛型约束 |

## 18. 本项目最重要的 Rust 编码规则

1. nominal ID，不使用通用 String 互换；
2. authority amount 不使用浮点；
3. 状态使用 enum + 穷尽 match；
4. recoverable failure 使用 typed Result；
5. Tokio/SQLite/Wasmtime 类型不进入公共 contract；
6. 所有 queue/channel 有界；
7. 不在数据库 transaction 内 await；
8. 不持锁跨 await；
9. unsafe 集中、注释并验证；
10. ownership 不能替代 durable CAS/fencing；
11. panic 表示程序 bug，不表示普通 provider/用户错误；
12. 每个 async external effect 绑定 OperationId。

## 19. 学习顺序

不需要先完整学完 Rust 再理解项目。建议顺序：

1. struct、enum、match；
2. ownership、borrowing；
3. Option、Result、`?`；
4. trait 和 module/crate；
5. Arc、Mutex、channel、Send/Sync；
6. async/await、Future、Tokio；
7. lifetime；
8. unsafe/FFI。

配合项目代码阅读会比纯语法学习更有效。官方《The Rust Programming Language》可在线阅读，也能通过 `rustup doc --book` 打开本地版本。[Rust Book](https://doc.rust-lang.org/stable/book/)
