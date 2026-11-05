# Meeting Master

macOS 上的图形化会议工作台：录屏、录音、日程、实时字幕与说话人识别、自动转写、会议纪要生成与管理，以及全模态搜索（文字、图片、音频、视频）。笔记和纪要写进你的 Obsidian 库；没有自带 API 密钥时，通过本机 Codex 使用你的 ChatGPT 账号生成纪要。

> 当前状态：**v0.1 仓库骨架**。技术栈已确定，依赖已安装，各模块的接口和核心算法有测试，界面原型已完成。录制、字幕、嵌入等功能的完整链路还没接通，见下方“下一步”。

## 快速开始

```bash
scripts/bootstrap.sh
```

```bash
make test
```

```bash
make engines
```

```bash
make run
```

界面原型直接用浏览器打开 [design/mockups/index.html](design/mockups/index.html)。

## 目录

| 路径 | 内容 |
| --- | --- |
| [crates/mm-app](crates/mm-app) | GPUI Kit 桌面应用（目前是导航外壳） |
| [crates/mm-core](crates/mm-core) | 领域模型：会议、人物、声纹、字幕事件 |
| [crates/mm-engine](crates/mm-engine) | 与 Swift 引擎通信的 JSON-RPC 客户端 |
| [crates/mm-index](crates/mm-index) | 自动索引：稳定分块、差异比对、防抖、媒体切窗 |
| [crates/mm-store](crates/mm-store) | SQLite + sqlite-vec + FTS5 |
| [crates/mm-vault](crates/mm-vault) | Obsidian 集成 |
| [crates/mm-llm](crates/mm-llm) | 纪要提供方选择、Codex app-server 请求、纪要 JSON Schema |
| [native/NativeKit](native/NativeKit) | Swift：JSON-RPC 服务端、GCC-PHAT 方位估计、说话人融合、麦克风探测 |
| [native/MeetingEngine](native/MeetingEngine) | `mm-meetingd`：录制、ASR、说话人识别、日程 |
| [native/EmbedEngine](native/EmbedEngine) | `mm-embedd`：EmbeddingGemma 2 多模态嵌入 |
| [design/mockups](design/mockups) | 第一版界面原型（HTML/SVG） |
| [docs](docs) | 文档：[技术栈](docs/tech-stack.md)、[架构](docs/architecture.md)、[说话人识别](docs/speaker-identification.md)、[引擎协议](docs/engine-protocol.md)、[方位可行性](docs/spatial-feasibility.md)、[iPhone 可行性](docs/ios-feasibility.md)、[iPhone 互联](docs/companion-link.md) |

## 下一步（建议顺序）

1. 下载模型权重：EmbeddingGemma 2 bf16（约 1.6 GB），Qwen3-ASR 0.6B/1.7B、Sortformer、WeSpeaker（合计约 3.3 GB）。
2. `mm-meetingd`：~~`capture/start` / `capture/stop`（窗口录制、麦克风多声道、系统声音、电平与状态通知）~~ 已完成；下一步接入 Qwen3-ASR 流式字幕与 Sortformer。
3. `mm-app`：把原型的“会议中”页面和浮窗字幕做成真实页面，并注册原型里的主题色。
4. 索引管线：vault 监听（FSEvents）→ `mm-index` → `mm-embedd` → `mm-store`。
5. 用 `mm-llm` 驱动 `codex app-server`，生成纪要并写回 Obsidian。
