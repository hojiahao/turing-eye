# R1.2 模型 SDK 验证

本目录用于独立验证模型 SDK 的兼容性，尚未实现 Runtime，也不修改线上模型配置、保存评测结果或连接业务存储。

## 依赖基线

| 依赖 | 固定版本 | 启用特性 |
| --- | --- | --- |
| rig-core | 0.42.0 | 关闭默认特性，启用 reqwest、rustls |
| reqwest | 0.13.5 | 关闭默认特性，启用 json、rustls |
| httpdate | 1.0.3 | 默认特性 |
| tracing | 0.1.44 | 默认特性 |
| jsonschema | 0.33.0 | 关闭默认特性 |
| tokio | 使用工作区版本 | 在已有特性上增加 net、signal |

上述依赖和验证程序通过 `dependency-probes` 特性启用。测试与真实调用使用同一份 `Cargo.lock` 锁定依赖。

Rig 0.42.0 官方发布包声明的库名为 **rig_core**，采用 Rust 2024 版本规范，但未声明 `rust-version`。其 `.cargo_vcs_info.json` 指向[源码提交 d5a3498](https://github.com/0xPlaygrounds/rig/tree/d5a34986a1ad57f1e9c5984b82f8d7438ffc717e)，该提交使用 Rust 1.94.0 工具链。本工程锁定的依赖已在 Staging 使用 Rust 1.93.1 编译，并通过模型故障样例回归。这只证明当前依赖组合可用，不代表后续依赖版本也支持相同的最低 Rust 版本。

适配器调用 Rig 的类型化方法 `raw_completion_with_request_id`，保留拒答、原始工具调用 ID 和可选用量数据的区别。请求由 Rig 构造并发送，不另写一套供应商 HTTP 客户端。注入的 reqwest 客户端关闭自动重试与重定向，也不启用 Rig 的重试客户端或 Agent 循环。部分 Rig 错误与追踪日志可能包含完整请求或响应，因此 SDK 异步调用在 `NoSubscriber` 下执行，避免记录原始正文。

## 启动配置

程序不读取 `.env`，也不提供默认模型或凭据。Staging 启动程序通过进程环境注入以下变量，不将其值输出到日志：

| 环境变量 | 含义 |
| --- | --- |
| R1_ALLOW_LIVE_PROBES | 必须为 `1`，否则在调用供应商前退出 |
| R1_TEXT_BASE_URL | Staging 文字模型服务的基础地址，包含 API 前缀，不包含 `/chat/completions` |
| R1_TEXT_API_KEY | Staging 文字模型服务凭据 |
| R1_TEXT_MODEL | 实际文字模型名称，已核实的部署配置为 deepseek-v4.1-flash |
| R1_VISION_BASE_URL | Staging 视觉模型服务的基础地址 |
| R1_VISION_API_KEY | Staging 视觉模型服务凭据 |
| R1_VISION_MODEL | 实际视觉模型名称，已核实的部署配置为 qwen3.8-max |
| R1_MODEL_TIMEOUT_SECONDS | 可选的单次调用超时，范围为 1 至 180 秒，默认 60 秒 |

现有部署的四个角色使用同一文字模型。本实验只验证文字与视觉两类 SDK 客户端，不验收角色路由或完整评测流程，也不读取旧开发环境的 `.env.example`。

在 `server/` 目录执行：

```sh
cargo test --locked -p turing-eye-service --features dependency-probes infrastructure::models::tests -- --test-threads=1
R1_ALLOW_LIVE_PROBES=1 cargo run --locked -p turing-eye-service --features dependency-probes --bin probe_models
```

编译和真实调用在约定的 Staging 隔离环境执行，避免占用开发机有限的磁盘。测试命令使用回环 HTTP 服务和虚构凭据，无需真实密钥。验证程序会访问实际模型服务并消耗 Token。

## 验证内容与边界

真实实验最多调用 SDK 八次，不自动重试。每个模型分别验证文字或图片输入、严格结构化输出、两个并行工具调用，以及回传工具结果后的继续生成。图片使用生成的红蓝 PNG，不传输选手材料。工具结果故意按相反顺序回传，仍须通过 ID 关联正确值。若供应商只返回一个工具调用，并行能力检查失败，后续回传检查不执行，程序不会伪造第二个调用。

标准输出使用 JSON Lines，记录用例、模型类别、通过或失败状态、耗时毫秒数、SDK 调用次数、经校验的用量或 null、工具调用次数、是否返回请求 ID，以及分类后的错误。输出不包含供应商正文、提示词、隐藏推理、工具参数与结果、凭据、服务地址或请求 ID 原值。标准错误只输出固定的结果写入错误标识。退出码 0 表示八项兼容检查全部通过，1 表示存在失败或阻塞，2 表示结果输出失败。收集输出时必须保留退出码。

用量完整性与模型输出是否有效分别记录。缺少输出 Token 时保留 null，不用 `total - input` 推算；整份用量缺失时同样保留 null，不补零。缓存输入不重复累加。只要有调用用量未知，已知 Token 合计就只是小计。这里统计的是评审服务调用成本，不是选手开发成本。

用量数据先做数值一致性检查。不合格的数据记为 null，计入用量未知的调用，不计入已知 Token 合计。拒答、截断或其他输出失败只在用量校验通过时保留用量，不改变原本的错误分类。

Rig 0.42.0 将缺失的 `prompt_tokens_details.cached_tokens` 默认为零，其类型化响应无法区分“未提供”与“明确为零”。R1 暂将两种情况都映射为 `cached_input_tokens: null`，非零值则保留，并校验其不超过输入 Token。这会丢失对真实缓存未命中的确认，但不改变输入、输出或总 Token。`usage_complete` 表示输入、输出与总量是否完整，不表示缓存计数一定可用。调用仍通过 SDK 完成。

故障样例覆盖 401、403、400、429 与 Retry-After、5xx、上下文限制、非法 JSON 或结构、不合规工具调用、拒答、截断、用量缺失、超时及取消。样例通过不代表真实网关必然返回相同错误结构。实验不向真实供应商发送拒绝服务、超大上下文或故意触发拒答的请求。

Rig 0.42.0 的聊天响应解码失败可能被包装成 HTTP 2xx 的 `ProviderResponse`。适配器只在内存中检查保留的 JSON，将工具参数中的非法 JSON 归类为 `invalid_json`。供应商明确返回的错误对象和非成功 HTTP 状态保留原分类，并保留状态码、是否存在请求 ID 及 Retry-After，不将原始正文或参数写入错误结果。SDK 解码失败时用量仍记为未知。回归样例覆盖 SDK 原始包装行为及两条分类路径。

取消只停止本地等待，不能证明供应商已停止处理或计费。发送前取消时 SDK 调用次数为零；发送后取消且未收到响应时，远端执行结果未知。取消控制方消失时也会停止本地等待。失败调用不自动重试。

R1 尚未验收真实上下文窗口大小、任意多模态格式、流式响应、完整 Runtime 权限与预算、响应流大小限制、性能、高可用或生产发布条件。供应商与工具的完整编排按后续已评审批次实现。
