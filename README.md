# Meeting Master

macOS 上的图形化会议工作台：录屏、录音、日程、实时字幕与说话人识别、自动转写、会议纪要生成与管理，以及全模态搜索（文字、图片、音频、视频）。笔记和纪要写进你的 Obsidian 库；没有自带 API 密钥时，通过本机 Codex 使用你的 ChatGPT 账号生成纪要。

> 当前状态：**v0.2 macOS 版功能完整**。录制、实时字幕与说话人、会后精转写、纪要生成（BYOK / Codex / 本地模型）、Obsidian 写回、自动索引和多模态搜索均已接通，六个页面和浮窗字幕可用。尚待真人验证的部分见文末“已知限制”。

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
scripts/dev-app.sh launch
```

发布包（Release 引擎 + 优化编译）：

```bash
scripts/package-app.sh
```

界面原型直接用浏览器打开 [design/mockups/index.html](design/mockups/index.html)。

## 目录

| 路径 | 内容 |
| --- | --- |
| [crates/mm-app](crates/mm-app) | GPUI Kit 桌面应用：今天、日程、会议中、纪要库、说话人、设置，浮窗字幕 |
| [crates/mm-service](crates/mm-service) | 无界面服务层：设置、引擎调用、字幕状态、会议流程、后台索引 |
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

## 首次使用

1. 打开 App 后在“设置”里确认 Obsidian 库（会自动发现）。
2. 第一次开始记录时，macOS 会请求麦克风、屏幕录制（录窗口和系统声音）和日历权限，需要逐一允许。
3. 实时字幕模型（Qwen3-ASR 0.6B、Silero VAD、WeSpeaker，约 0.7 GB）第一次录制时自动下载到 `~/Library/Caches/qwen3-speech`。
4. 搜索的语义部分需要在“设置”里选择 EmbeddingGemma 2 文件夹并加载；没有它时只用关键词检索。
5. 纪要提供方：配置了 API 密钥就用 BYOK；否则用已登录的 `codex`（ChatGPT 账号）；都没有时可以加载本地模型（如 Qwen3-4B-Instruct 4bit）。

## 已知限制

- 没有屏幕录制权限时，开始记录会自动降级为只录麦克风，并提示去系统设置里授权；授权后才会录窗口和系统声音。
- 系统声音这一路的“采集”本身需要屏幕录制权限，自动化测试环境里没有；其后的实时字幕管线已用 `caption/simulateFile`（`origin: system`）验证。
- 方位识别需要 ≥2 声道的外接麦克风（见 [说话人识别](docs/speaker-identification.md)）。
- 会后的 Pyannote 聚类阈值沿用上游默认值，音色相近的人可能被合并，建议用真实会议调参。
