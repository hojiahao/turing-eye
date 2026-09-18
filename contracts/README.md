# Agent 评审平台接口契约

版本：1.0，2026-09-18。这是新产品的开发基线，**尚未实现或上线**。线上 `/v1` 不改路径、字段、单位或状态。本契约使用独立 `/v2`，不得让平台提前切换生产调用。

**本轮评审修正：** `/v2` 创建及预览用 `plan_revision` 明确方案版本；评分快照包含无分时的缺项状态；指标增加 evidence_refs，汇总保留指标明细；汇总来源纳入 commentary_revision。调整只作用于尚未发布的新契约，旧 `/v1` 不增删或重命名字段。

## 契约入口

| 文件 | 唯一职责 |
| --- | --- |
| [openapi.json](openapi.json) | 24 个 HTTP 操作的完整请求、响应、类型、空值、权限及事件载荷；OpenAPI 3.1.1 / JSON Schema 2020-12 |
| 本文 | Schema 不能表达的跨字段关系、事务、并发、错误与恢复语义 |
| [工具协议](工具协议.md)及 [Schema](tool-protocol.schema.json) | Rust 宿主与 Node 浏览器工具的内部协议，不是平台 HTTP API |
| [契约样例](examples.json) | 可重复验证的有效及无效对象；不冒充真实接口联调 |
| [技术方案](../docs/Agent评审平台技术方案.md) | 模块、数据与执行架构；业务公式和质量门槛只引用 [PRD](../docs/Agent评审平台PRD.md) |

外部规范采用 [OpenAPI 3.1.1](https://spec.openapis.org/oas/v3.1.1.html)、[HTTP 语义](https://www.rfc-editor.org/rfc/rfc9110.html)和 [Problem Details](https://www.rfc-editor.org/rfc/rfc9457.html)。版本选择服务于当前契约工具，不宣称是最新标准版本。

## 身份、请求与数值

所有接口均需 `Authorization: Bearer <对应租户的服务凭据>`。有 JSON 正文时使用 `Content-Type: application/json`，通常接受 `application/json`；错误为 `application/problem+json`，事件流为 `text/event-stream`。不要求 Referer。浏览器经平台后端会话代理，不能持有服务凭据。

| 约定 | 规则 |
| --- | --- |
| ID | `/v2` 的方案、Run、Job、产物及作品标识使用字符串，不把长数字转换为 JavaScript Number。`/v1` 原类型不变 |
| 维度与指标键 | 小写英文、数字及下划线，字母开头，最多 64 字符；不是固定枚举。维度数量 N≥1，仅受正文大小限制，不限五维 |
| 比例 | 严格四位小数字符串，`"0.2000"` 为 20.00%；每维 AI/人工之和及全部维度权重之和必须精确等于 1.0000 |
| 分数 | JSON 数字 0..100，最多两位小数；0 有效，null 表示尚无分。后端十进制解析为整数百分之一分，禁止二进制浮点求和 |
| 时间 | RFC 3339，必须带时区；页面按用户所在时区展示，不能以页面刷新时间替代记录时间 |
| 文本 | 标准、标题等必填文本不得全为空白。人工评语可省略或 null；提供字符串时不得全为空白。禁止 C0 控制字符（保留换行、回车和制表）及 DEL，不限制语言比例，原文不自动修辞 |
| 空值 | 必填 nullable 响应字段始终存在；可选请求字段可省略。源码二选一中未采用的字段省略，不传 null。人工提交是完整替换，省略评语即保存 null，不暗中继承上一版 |
| 大小 | JSON 请求体上限 2 MiB；批量 1..100 项。源码下载包 500 MiB、单个解压文件 128 MiB、总解压 1 GiB，另做路径/文件数/解压安全控制，不能按大小代替质量判断 |
| URL | HTTP/HTTPS，实际连接须通过出网策略。禁 URL 用户名/密码；签名 query 加密保存、不进普通日志。样例域名与摘要均是假值 |

模型、页面或服务配置不得填写缺失的维度比例。内部指标权重由方案生成、编辑并定稿，不与平台维度比例混用。所有响应按平台保存的维度顺序返回，不能字母排序或补入默认维度。

客户端忽略不影响已知语义的新增响应字段；写请求拒绝未知字段，避免拼写错误被静默忽略。新增必填项、修改单位或已有枚举语义必须升级契约，不能借重构破坏旧调用方。

## 操作与权限

以下路径省略 `/v2`。`K` 表示必需 Idempotency-Key，`E` 表示必需 If-Match；没有正文的操作不接受模型指令或隐式配置。

| 方法与路径 | 权限 | 请求 → 成功响应 | 写入保护 |
| --- | --- | --- | --- |
| POST /plans | plans.write | CreatePlanRequest → 202 PlanJobReceipt | K |
| GET /plans | plans.read | page/page_size/q/status → 200 PlanPage | 只读 |
| GET /plans/{plan_id} | plans.read | 可选 revision → 200 Plan + ETag | 只读 |
| PUT /plans/{plan_id}/draft | plans.write | SavePlanDraftRequest → 200 Plan + ETag | K、E |
| POST /plans/{plan_id}/generate | plans.write | 无正文 → 202 PlanJobReceipt | K、E |
| POST /plans/{plan_id}/finalize | plans.write | 无正文 → 200 Plan + ETag | K、E |
| POST /reviews/preview | reviews.create | PreviewRequest → 202 JobReceipt | K，只解析 |
| POST /reviews | reviews.create | CreateReviewRequest → 202 ReviewReceipt | K |
| GET /reviews | reviews.read | page/page_size/q/state/submission_id → 200 ReviewPage | 只读 |
| POST /reviews/batch | reviews.create | items: CreateReviewRequest[] → 202 BatchCreateResponse | K |
| POST /reviews/batch/status | reviews.read | run_ids: string[] → 200 BatchStatusResponse | 只读，不要求 K |
| GET /reviews/{run_id} | reviews.read | 无正文 → 200 ReviewStatus | 只读 |
| POST /reviews/{run_id}/cancel | reviews.control | 无正文 → 200 ReviewStatus | K |
| POST /reviews/{run_id}/retry | reviews.control | RetryRequest → 202 RetryReceipt | K |
| GET /reviews/{run_id}/scores | scores.read | 无正文 → 200 Scores | 只读 |
| POST /reviews/{run_id}/human-results | human_results.write | HumanResultsRequest → 202 HumanResultsReceipt | K、expected_revision |
| GET /reviews/{run_id}/human-results | human_results.read | 无正文 → 200 HumanResults | 只读 |
| GET /reviews/{run_id}/summary | summary.read | 无正文 → 200 Summary | 只读 |
| GET /reviews/{run_id}/publication | reports.ai.read 或 reports.summary.read | report_type=ai（默认）/summary → 200 Publication | 只读 |
| GET /reviews/{run_id}/evidence | evidence.read | page/page_size/kind → 200 EvidencePage | 只读 |
| GET /artifacts/{artifact_id}/content | 按所属对象及产物种类检查 | download、If-None-Match → 200 正文 / 304 | 只读 |
| GET /reviews/{run_id}/events | reviews.read + events.read | cursor/limit → 200 EventPage | 只读 |
| GET /reviews/{run_id}/events/stream | reviews.read + events.read | cursor/Last-Event-ID → 200 SSE | 只读 |
| GET /jobs/{job_id} | 按任务归属检查 | 无正文 → 200 Job | 只读 |

租户和资源范围来自已验证身份，不接受正文指定 tenant_id。跨租户对象按不存在返回；同租户但缺少操作权限返回 403。Job 查询不能成为绕过权限的入口：方案任务需要 plans.read 或其接纳者的 plans.write，预览需要 reviews.create 且属于该调用来源，评测需要 reviews.read；文案/发布按评分或报告种类，汇总按 summary.read 或该人工提交回执来源校验。只有人工写权限时只返回更新 Job 的状态与受控引用，不附 AI 分/汇总正文。引用的后续 GET 仍重新授权。

事件再按种类投影：证据需要 evidence.read，评分/文案需要 scores.read，人工修订需要 human_results.read，汇总需要 summary.read，发布按报告类型。未授权事件整体过滤，不能只删 data 却留下泄漏内容的 summary；游标可跨过不可见事件。取消/恢复回执同样不返回受限评分正文。

## 方案与定稿

首次 POST 只需要赛题和每维五字段，不要求指标。生成 Job 结束后仍是草稿。初始 `revision=1`、`draft_version=1`；同一草稿每次成功保存或生成结果提交递增 draft_version。定稿冻结 revision；其后的编辑从下一 revision 开始。服务返回强 ETag，编辑方原样回传，不自行拼接，不允许 `*`。

PUT 是完整替换：平台维度比例必须合法；指标允许列表为空、文本未填或内部比例尚未配齐，响应以 `validation_issues` 指明位置。新增指标可省略 indicator_key，由服务分配；既有指标保留原键，不能通过换数组下标重建身份。提供的非空键须属于同维当前草稿或其源定稿；跨维移动视为删除后新增，未知键返回 422。删除不改旧修订与已有 Run。

生成恢复只处理尚无指标的维度，整个维度候选校验通过后原子提交；失败维度保持空并记录原因，不把半个指标当完成。已有但未写完的指标由编辑入口继续填写。输入有变更时，旧 Job 可结束但失去覆盖权；模型漏、多造维度或修改平台比例不得定稿。

定稿仅检查当前修订、无活动生成任务、每维有指标、键与归属、必要文本、指标内权重和为 1.0000、平台配置仍成立。失败不触发外部任务。服务为规范 JSON 内容计算摘要（对象键排序、数组顺序保留、比例字符串原样），返回完整 Plan；创建 Run 时把 Plan.revision 传入 plan_revision，并带同一 plan_id/content_sha256，不引用最新草稿。

## 幂等与批量

所有有副作用的操作都需 K。幂等身份为租户、调用来源、HTTP 操作与资源路径、键；指纹包含规范请求正文及有业务意义的前置修订，不包含日志 request_id。键在该身份下不可复用。

处理顺序为鉴权 → 基本格式校验 → 查同键回执/指纹 → 新请求的前置版本与业务校验 → 短事务提交。相同请求返回原业务回执，即使其 expected_revision 已过时；同键不同内容返回 409，不覆盖原结果。传输失败按原键重试，不能自动换键。并发首次请求由唯一约束保护；处理中返回 `IDEMPOTENCY_IN_PROGRESS` 和 Retry-After。

接纳回执随业务记录保留；归档后仍保留键与结果归属，不能只按短 TTL 删除后允许重复创建。将来若删除归档数据，应留拒绝复用的墓碑，不把结果遗失伪装成未接纳。读取回执仍检查当前权限。

批量条目完全复用单条字段，不加 item_key。身份从批次键和位置派生，完整有序请求先冻结。重复 submission_id 可表示同作品合法新评测，不以它去重。重放同批复用成功/业务拒绝回执，公共故障后补未处理项；改变顺序或正文需新批次键。响应按原顺序，计数和等于 items 长度。结构错误整批 422；单项不存在、未定稿等业务拒绝保留整批 202；数据库等公共故障可返回 503，不把系统异常伪装成业务拒绝。批次返回与某个 Run 完成是两回事。

单条与批量不同幂等身份，不承诺自动跨入口去重；平台不能把未知结果的单条请求换成批量重发。同一次重测使用新键与新 Run，局部恢复使用原 Run。

## 人工接收、评分与报告

人工完整提交覆盖所有 `W×h>0` 维度；可附 `W=0 且 h>0` 的维度，但不能附 `h=0`、未知或重复维度。缺必需分为 422，0 分合法。纯 AI 配置没有可提交的人工集合时返回 `HUMAN_RESULTS_NOT_REQUIRED`。平台负责评委归并，我方只接收每维一个当前有效结果，不从身份推算平均分。

首次提交 expected_revision 为 0；后续从 GET 取得当前人工 revision，并用新幂等键完整提交。CAS 检查不通过为 409 `HUMAN_REVISION_CONFLICT`。人工分和原文、递增修订、回执、汇总 Job 同事务保存，再立即返回 202。AI 未完成不妨碍接收；提交不能改权重或 AI 分。

| 状态对象 | 必须满足的关系 |
| --- | --- |
| 指标 scored | score 为有效数字，rationale 非空且必要依据已校验入库；comment 可空，不影响有效分 |
| 指标 pending / unavailable / not_required | score 为 null。必要项暂未执行为 pending，无法判定为 unavailable，规则不要求执行为 not_required；不得把缺分标成有效零分 |
| 维度 / AI complete | 所有必需正权重指标齐备，相应 ai_score 非空；其他状态总分为空，已有效指标仍返回 |
| 全人工 | Run/AI 评分/AI 文案/AI 发布为 not_required，AI 分与自动 job_id 为空；不下载材料或启动空自动 Job |
| summary pending / partial / complete | 尚无必需有效结果 / 部分已取得 / 必需两路全部齐备；只有 complete 有 final_score。GET 不补算或入队 |
| decision | 仅完整分且方案显式给出合法阈值时产生 pass/conditional_pass/fail；未配置或无完整总分为 null，不默认 80/50 |

score_revision 标识完整评分快照：包括有效分，也包括 pending/unavailable 等状态和缺项原因。首次尚无快照或无需 AI 时才为空；从等待变成失败、取消或恢复时，缺项变化也须保存新修订，同事务登记汇总及 AI 报告更新意图。报告按实际有效指标决定评分展示或诊断，不能根据修订号非空判断有分，也不为全无分的快照发评分润色任务。

IndicatorScore.evidence_refs 是判定所用 ready 产物 ID 数组，绑定该评分快照；内容仍经证据权限读取，页面不必直接展示机器标识。SummaryDimension.indicators 保留相同来源的指标明细，维度缺分时已有指标分不能隐藏。不把人工维度评语分摊成指标评价。

汇总 query 返回已经保存的来源 `source_revisions`、当前有效来源 `current_source_revisions`、is_latest、更新 Job 与错误。来源含 plan_revision、score_revision、human_revision、commentary_revision，任务去重与条件提交使用同一完整组合。尚无汇总修订时 summary_revision=null、final_score=null，返回冻结维度与待算状态；不把已提交未聚合的值冒充已保存汇总。新来源到达时旧汇总仍可读但 is_latest=false。已计算且等待必要评分的 partial 汇总可以 is_latest=true，含义是采用了当前输入，不是分数完整。

发布按 `report_type` 隔离。ReportSource 同时绑定规则、评分、人工、汇总与文案修订；不适用的修订为空。AI 报告 human_revision/summary_revision 必须为空。人工更新只改变汇总，不使 AI 报告过期。公开文案保存时同时登记两类后续更新意图；汇总只有文案变化时复用已存数值，生成新展示快照与报告，不重新计分或调用模型。所选文案必须与 score_revision 匹配，没有匹配文案时先用 null/已存依据，不等待润色。已有判定不变时可复用其文案，不能因缺项状态更新重复请求模型。

`publication_status` 描述当前目标，`published` 是最近可读产物。published.is_latest 由当前查询计算，不写进不可变正文；等待、失败都可保留旧链接。汇总正文使用 SummarySnapshot，禁止保存“当前来源、更新 Job、是否最新”这些动态字段。正文中所有状态是该报告生成时的快照，实时进度以状态接口为准。JSON 与 HTML 引用完全相同的 source；只在两种产物均 ready 后推进发布指针。

首次没有产物为 published=null，正常 200；评分 failed/partial 也不能让查询返回 422。只有无有效分才展示诊断内容，不能因 URL 不可达隐藏已有源码分。报告正文的身份/修订字段供程序定位，阅读模板不把这些内部字段堆进自然语言正文。

### 局部恢复

| target | 可以做 | 不能做 |
| --- | --- | --- |
| evaluation | 按已保存事实补失败取证或未形成有效结果的判定 | 重评已有效指标、换源码/URL/方案或重置累计预算 |
| commentary | 为当前有效评分补失败公开文案 | 调浏览器、改分、覆盖人工原文 |
| summary | 从已入库的两路结果恢复失败聚合并保存发布意图 | 再次评测、调用模型、把正常等待人工输入当失败重试 |
| publication | report_type 必填，仅恢复该类型的渲染与存储 | 汇总未生成时越界重算、重评或修补证据 |

只有缺失/失败且可恢复时接纳；活动任务按同一目标复用，没有可恢复工作返回 409。回执的 source 绑定接纳时来源，stages 列出实际范围。旧来源迟到不能覆盖新值；来源更新触发的新 Job 不因旧回执重放而取消。

取消只取消自动执行及其未完成动作，保留已保存评分、人工结果和可发布内容；取消已完成/未要求的自动执行返回实际状态，不伪造 cancelled。文案和报告是独立任务，关闭订阅或浏览器不取消任何业务。全流程重测必须新建 Run，不继承原人工结果。

## 事件、证据与分页

RuntimeEvent 的全部 11 种 kind 及对应 data 在 OpenAPI 统一定义，不另复制一套事件 Schema。tool.started 的耗时和 error 为空；tool.completed 有非负耗时、error=null；tool.failed 有正式错误和非负耗时。artifact_ids 只能引用 ready 证据。发布事件只有 published 时含非空 report_revision。

JSON 事件页默认 50、最大 200 项，正文至多 256 KiB，单事件至多 16 KiB。SSE 的 review_event 帧包含同一对象，id 为签名续读游标；stream_control 不含 id。Last-Event-ID 优先于旧 query cursor。断线按 Run+sequence 去重，读当前一致性快照后续读，不从事件文字计算业务状态。

在线事件保留 7 天、心跳 15 秒、连接 10 分钟轮换、每订阅缓冲至多 256 项或 1 MiB，作为首轮可配置起值，不是容量上限证明。过期 410 后读取当前状态/评分/报告，再按新游标订阅；正文历史保留不受事件窗口影响。首个 HTTP 头发出后只能发送控制事件或关闭，不能伪造新的 HTTP 状态。慢订阅不得阻塞 Worker、占用长期事务或每连接独占数据库连接。

方案、Run、证据列表以数据库授权过滤后分页，默认 page=1/page_size=20、最大100；按响应中实际页显示，超末页收敛至最后有效页，空列表为第一页。total 与 items 用一致性快照查询，total 不包含不可见记录。q 作用于名称和标识，不搜索秘密 URL。批量创建响应最多100项不再分页，工作台持有其回执关联并分页展示；进入单条或刷新状态复用 Run 查询，不创建第二套批次执行状态。

证据列表只回元数据和 `/v2/artifacts/{id}/content`；源码导入完成后状态中返回 source_artifact_id。artifact_id 复用仅限本租户可见、与本 submission_id 绑定且 ready 的源码包，不接受任意 S3 键或旧 `/v1` 数字 ID。首次通过 payload_url 导入即可，无需新增上传平台或空的导入服务。

内容读取每次先授权。条件 GET 的 304 没有正文，不是错误；缓存按租户、产物、修订和权限范围隔离。报告 HTML 来自可信模板，启用 CSP/nosniff；作品 HTML/DOM 以纯文本或附件读取，不能在评审域执行。JSON 证据以附件二进制返回，报告 JSON 才按 ReportDocument 解析。文件缺失返回准确错误，不在 GET 修补。

## HTTP 错误

所有失败响应使用 OpenAPI Problem；errors 为空数组或 `{code,field,message}` 明细，field 使用如 `dimensions[1].weight_ratio` 的实际路径，不返回原始秘密输入。请求级错误和已接纳任务失败分开。

| HTTP | 稳定 code | 场景 / 处理 |
| --- | --- | --- |
| 400 | MALFORMED_JSON / INVALID_EVENT_CURSOR | 无法解析正文 / 游标格式、签名或归属不符；修正请求 |
| 401 | AUTHENTICATION_REQUIRED / INVALID_CREDENTIALS | 缺少或失效凭据；响应含 WWW-Authenticate: Bearer |
| 403 | PERMISSION_DENIED | 租户可见但操作权限不足；不得重试绕过 |
| 404 | PLAN_NOT_FOUND / RUN_NOT_FOUND / JOB_NOT_FOUND / ARTIFACT_NOT_FOUND | 对象不存在或不属于可见范围 |
| 405 | METHOD_NOT_ALLOWED | 路径不支持该方法，含 Allow |
| 406 | NOT_ACCEPTABLE | 无法提供请求 Accept 的媒体类型 |
| 409 | IDEMPOTENCY_CONFLICT / IDEMPOTENCY_IN_PROGRESS | 同键异体 / 同键处理中；后者可按 Retry-After 重试 |
| 409 | PLAN_NOT_FINALIZED / PLAN_DIGEST_MISMATCH | 引用非定稿 / 摘要与所引修订不符；不可自动换新方案 |
| 409 | PLAN_DRAFT_REQUIRED / PLAN_DRAFT_INCOMPLETE / PLAN_GENERATION_IN_PROGRESS / PLAN_NOTHING_TO_GENERATE | 无草稿 / 未完成 / 正在生成 / 无需补生成；按缺项恢复，不触发隐藏生成 |
| 409 | HUMAN_REVISION_CONFLICT / HUMAN_RESULTS_NOT_REQUIRED | 旧人工修订 / 配置未要求人工结果 |
| 409 | RETRY_NOT_ALLOWED / ARTIFACT_NOT_READY | 没有合法恢复工作 / 已有可见产物尚未 ready |
| 410 | EVENT_CURSOR_EXPIRED / ARTIFACT_EXPIRED | 在线事件窗口过期 / 产物已按保留策略删除；不能用 410 隐藏意外丢失 |
| 412 | REVISION_CONFLICT | If-Match 已过时，先读取当前内容 |
| 413 | REQUEST_TOO_LARGE | HTTP JSON 正文超限；与后台源码包超限不同 |
| 415 | UNSUPPORTED_MEDIA_TYPE | 非法 Content-Type |
| 422 | REQUEST_VALIDATION_ERROR | 字段/类型/范围/结构不合法；errors 用下表的精确原因 |
| 428 | PRECONDITION_REQUIRED | 必需 If-Match 未提供 |
| 429 | RATE_LIMITED / QUEUE_CAPACITY_EXCEEDED | API 请求配额 / 可接纳任务预算已满；含 Retry-After；不是供应商 429 |
| 500 | ARTIFACT_INTEGRITY_ERROR / INTERNAL_CONTRACT_ERROR / INTERNAL_ERROR | 已登记产物意外缺失或损坏 / 服务自身契约矛盾 / 未识别异常；日志保留根因，不让用户改参数 |
| 503 | DATABASE_UNAVAILABLE / STORAGE_UNAVAILABLE | 必要依赖暂不可用；同幂等键有界重试，不伪称未接纳 |

422 明细 code：`REQUIRED_FIELD`、`UNEXPECTED_FIELD`、`INVALID_TYPE`、`INVALID_VALUE`、`OUT_OF_RANGE`、`INVALID_PRECISION`、`INVALID_TEXT`、`DUPLICATE_DIMENSION_KEY`、`DUPLICATE_INDICATOR_KEY`、`INVALID_DIMENSION_WEIGHTS`、`INVALID_CHANNEL_WEIGHTS`、`INVALID_INDICATOR_WEIGHTS`、`UNKNOWN_INDICATOR_KEY`、`UNKNOWN_DIMENSION_KEY`、`SOURCE_REQUIRED`、`SOURCE_CONFLICT`、`SOURCE_BINDING_MISMATCH`、`HUMAN_DIMENSION_REQUIRED`、`HUMAN_DIMENSION_NOT_ALLOWED`、`INVALID_DECISION_THRESHOLDS`。指标草稿未完成以 Plan.validation_issues 表达；相同明细不能在合法草稿保存时误变成422。

预览的可修正问题通过 issues 返回，包括 `PREVIEW_CONTEXT_CONFLICT`、`PLAN_EDIT_REQUIRED`、`PLAN_NOT_FINALIZED`、`SOURCE_REQUIRED`。解析 Job succeeded 只表示完成了候选分析，can_create=false 仍不能创建。模型失败才使该 Job failed。

## 已接纳任务的错误

TaskError 使用 code/stage/category/retryable/detail/request_id；查询本身正常时 HTTP200。下列正式原因用于失败步骤或局部缺项，不因一个步骤失败抹掉全部结果。

| code | 分类及归属 | 恢复原则 |
| --- | --- | --- |
| SOURCE_DOWNLOAD_FAILED / SOURCE_URL_EXPIRED | import，input 或 provider，依据实际响应 | 网络暂故障预算内重试；材料变更须新 Run，不在原 Run 偷换 URL |
| SOURCE_ARCHIVE_INVALID / SOURCE_LIMIT_EXCEEDED | import，input | 明确格式或大小，修正材料后新建；不把已取得的其他证据抹掉 |
| SOURCE_ANALYSIS_FAILED | collect，service | 隔离解析故障，保留其他证据；修复后仅补分析 |
| ENVIRONMENT_UNREACHABLE | collect，input 或 provider | 记录 HTTP/连接的脱敏原因；不自动断言选手代码错误 |
| BROWSER_CAPTURE_FAILED / EVIDENCE_COLLECTION_FAILED | collect，service | 局部恢复，不能写成作品0分 |
| MODEL_RATE_LIMITED / MODEL_UNAVAILABLE / MODEL_TIMEOUT | planner/preview/collect/judge/commentary，provider | 按预算退避；供应商429不等于API接纳429 |
| MODEL_OUTPUT_INVALID | 对应模型阶段，provider | 有界修正/重试，仅影响该阶段；Judge 无有效判定不造分 |
| EVIDENCE_NOT_READY / ARTIFACT_INTEGRITY_ERROR | collect/judge/render，service | 不引用未登记证据；已保存评分无需渲染时再次取证 |
| STORAGE_UNAVAILABLE / DATABASE_UNAVAILABLE | 实际阶段，service | 已提交结果保留，按保存/发布边界恢复 |
| SUMMARY_COMPUTATION_FAILED / REPORT_RENDER_FAILED | summary / render，service | 只恢复聚合 / 渲染；数值输入不重评 |
| TASK_BUDGET_EXHAUSTED / TASK_DEADLINE_EXCEEDED | 实际阶段，budget | 不自动增加预算，保留已完成部分 |
| TASK_CANCELLED | 实际阶段，cancelled | 停止新动作，已有结果可读 |
| INTERNAL_CONTRACT_ERROR / INTERNAL_ERROR | 实际阶段，service | 已知内部不变量冲突 / 未识别异常；记录并修复，不重命名为作品缺陷 |

等待中的 missing_scores 使用 `SCORE_PENDING`、`HUMAN_RESULT_PENDING`；未要求的项不进入缺项表。必要证据确实不足可用 `EVIDENCE_INSUFFICIENT`，必须记录覆盖范围，不把它视为真实零分。已确认的作品缺陷保存在有效判定依据中，通常不需要把 Run 标成 failed。错误类别由实际根因决定，不能仅凭一个 HTTP404或超时一律归责选手。

## 开发与验收边界

本轮验证 Schema、样例和文档对应关系；R1 完成真实 SDK/PG/S3/工具协议实验，R2-R7 实现上述操作，R8 按 PRD 验收质量、容量及恢复。每个实现 PR 同步生成类型、契约样例和兼容差异。契约检查不能替代请求实际落库、网络调用、浏览器取证或评分质量验收。
