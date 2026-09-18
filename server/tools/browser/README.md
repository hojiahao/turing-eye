# R1 浏览器工具验证

本 TypeScript/Playwright 进程使用 `fixtures/game.html` 验证 [JSONL 2.0 工具协议](../../../contracts/工具协议.md)。实验仅允许已配置的本地样例地址 `http://127.0.0.1:<port>`、绑定的调用归属，以及 `/`、`/game`、`/slow`、`/missing`、`/state` 固定路径，不开放任意网站、公网子资源或 WebSocket。

| 工具 | R1 验证行为 |
| --- | --- |
| `open` | 创建隔离的浏览器上下文或跳转其主页面，多个上下文共享一个 Chromium 进程。 |
| `observe` | 读取有长度限制的 DOM 文本，或将当前视口保存为已遮盖敏感内容的 PNG。 |
| `mouse` | 执行点击、移动、按下、抬起或滚轮输入。 |
| `keyboard` | 执行按键、按下、抬起或文本输入。 |
| `console` | 按游标读取数量受限且已脱敏的控制台记录。 |
| `network` | 按游标读取请求元信息和失败记录，不返回请求或响应的正文、头部。 |
| `cancel` | 接收取消请求并开始关闭会话，接收成功不等于已经终止。 |
| `close` | 确认属于该调用方的浏览器上下文已经关闭。 |

在本目录使用 Node 24 和 `package-lock.json` 锁定的依赖执行：

```sh
npm run check
CHROMIUM_PATH=/root/.cache/ms-playwright/chromium-1243/chrome-linux64/chrome npm test
```

`npm test` 先编译，再以 TAP 格式输出结果。测试使用已有 Chromium，不下载浏览器。测试负责启动和关闭本地 HTTP 样例服务、Node 与 Chromium 进程，不进行压测。通过 Linux `/proc` 核对采样到的子孙进程是否全部退出，包括僵尸进程。清理只涉及该次测试创建的 Chromium 临时目录，已有证据文件保留。

真实浏览器测试验证 Canvas 像素及输入后的画面变化、Cookie 与 localStorage 隔离、八种工具、调用归属与执行尝试校验、取消、超时，以及输入流结束后的清理。独立的 JSONL 进程测试验证非法传输数据会被拒绝。单元测试覆盖消息分帧、响应关联和调用期限等于会话期限时的回归，这些单元测试不启动浏览器。

每次成功的生命周期测试保留 `.artifacts/r1-*/result.json`，PNG 截图保存在对应 `spool/` 目录。截图句柄、大小和 SHA-256 描述的是本地暂存文件，不代表证据已经上传或登记。关闭浏览器上下文后，尚未消费的截图仍保留。

本实验不代表 Rust 宿主联调、S3/数据库证据登记、业务 Runtime、评分、生产发布或容量验收通过。任意网站的出网安全、恶意页面隔离和进程崩溃恢复须在后续批次验收，不能仅凭浏览器路由拦截认定出网隔离有效。
