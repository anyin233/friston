<p align="center"><img src="crates/mm-app/assets/friston-icon.svg" width="128" alt="Friston 图标"></p>

# Friston

> 原名 Meeting Master。首次启动 Friston 时会把旧的数据文件夹、录音文件夹和钥匙串密钥迁移过来；Bundle ID 也换了，需要重新授权麦克风、录屏和日历。详见 `docs/architecture.md`。

macOS 上的图形化会议工作台：录屏、录音、日程、实时字幕与说话人识别、自动转写、会议纪要生成与管理，以及全模态搜索（文字、图片、音频、视频）。笔记和纪要写进你的 Obsidian 库；没有自带 API 密钥时，通过本机 Codex 使用你的 ChatGPT 账号生成纪要。

> 当前状态：**v0.1.0 macOS 版功能完整**。录制、实时字幕与说话人、会后精转写（本地 Qwen3-ASR / Codex 一体化 / OpenAI / Gemini）、纪要生成（BYOK / Codex / 本地模型，可选模型）、Obsidian 写回、自动索引和多模态搜索均已接通，六个页面和浮窗字幕可用。尚待真人验证的部分见文末“已知限制”。

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
| [crates/mm-llm](crates/mm-llm) | 纪要提供方选择、Codex app-server 请求、OpenAI / Google Gemini 云端转写与费率/账单、纪要 JSON Schema |
| [native/NativeKit](native/NativeKit) | Swift：JSON-RPC 服务端、云端实时流协议编解码、GCC-PHAT 方位估计、说话人融合、麦克风探测 |
| [native/MeetingEngine](native/MeetingEngine) | `mm-meetingd`：录制、本地 / 云端实时 ASR、说话人识别、日程 |
| [native/EmbedEngine](native/EmbedEngine) | `mm-embedd`：EmbeddingGemma 2 多模态嵌入 |
| [design/mockups](design/mockups) | 第一版界面原型（HTML/SVG） |
| [docs](docs) | 文档：[技术栈](docs/tech-stack.md)、[架构](docs/architecture.md)、[说话人识别](docs/speaker-identification.md)、[引擎协议](docs/engine-protocol.md)、[方位可行性](docs/spatial-feasibility.md)、[iPhone 可行性](docs/ios-feasibility.md)、[iPhone 互联](docs/companion-link.md)、[在线实时语音与免费额度调研](docs/online-asr-free-tier-survey.md) |

## 首次使用

1. 打开 App 后在“设置”里确认 Obsidian 库（会自动发现）。
2. 第一次开始记录时，macOS 会请求麦克风、屏幕录制（录窗口和系统声音）和日历权限，需要逐一允许。
3. 实时字幕默认使用本地模型（Qwen3-ASR 1.7B 4bit、Silero VAD、WeSpeaker、Sortformer，约 1.8 GB），第一次录制时自动下载到 `~/Library/Caches/qwen3-speech`；Hugging Face 缓存里已有 `mlx-community/Qwen3-ASR-1.7B-4bit` 时直接使用。在“设置 › 录制与字幕 › 实时字幕方式”里也可以切到 **OpenAI** 或 **Google Gemini**（可选 **实时流式（VAD 按需）**：Silero VAD + RMS 门控、懒连接与空闲自动断连；或 **按句上传（省流低成本）**：本机 Silero VAD 断句 + 可选中文 CPU Zipformer 即时草稿 + REST 按句上传），仍在本机做声纹、方位与说话人分离；保存密钥后自动拉取可用模型列表（支持手动刷新或自定义模型 ID），缺少密钥或连接失败时自动回退到本地 Qwen3-ASR。会后精转默认用本地 Qwen3-ASR 1.7B 8bit，也可在“精转方式”里切换到 **Codex 一体化**、**OpenAI 转写** 或 **Google Gemini**。实时字幕的延迟可以用 `scripts/eval-captions.py` 复测。
4. 搜索的语义部分需要 EmbeddingGemma 2。默认不用任何设置：启动时如果没填模型文件夹，App 先找本机已有的副本（自己的模型文件夹，或 Hugging Face 缓存 `~/.cache/huggingface/hub`），找到就直接加载，否则自动下载；可以在“设置 → 本地模型与搜索索引”关掉“启动时自动准备嵌入模型”。也可以在同一页手动点“下载并加载”，默认从 `https://huggingface.co/mlx-community/embeddinggemma-2-bf16` 下载（约 1.5 GB，可换成镜像地址，例如 `https://hf-mirror.com/…`），完成后自动加载；也可以直接填已有的模型文件夹。下载需要授权的模型时，在同一页保存 Hugging Face 令牌（存进钥匙串，没有时使用环境变量 `HF_TOKEN`）。没有嵌入模型时只用关键词检索。
5. 纪要提供方在“设置 › 纪要 › 纪要生成方式”里选：**自动**（配置了 API 密钥就用 BYOK；否则用已登录的 `codex`（ChatGPT 账号）；都没有时用本地模型，如 Qwen3-4B-Instruct 4bit），或指定 **API 密钥 / Codex**。指定的那一项用不了或失败时自动改用“自动”的顺序。在“设置 › 录制与字幕 › 精转方式”里选 **Codex 一体化** 时，将本地录音路径直接交给 Codex 一次完成转写、区分说话人和纪要生成。云端转写密钥（`openai`、`gemini` 与可选的 `openai-admin`）独立保存在 macOS 钥匙串中，并在“设置 › 云端转写密钥与花费累计”统计本机本月/累计转写时长与估算花费（OpenAI 可通过 Admin 密钥同步组织本月实际账单）。Codex 模型列表实时取自 `codex app-server` 的 `model/list`。

## 快捷键

菜单栏列出全部快捷键。它们在任何页面都可用，光标在输入框里时也一样（输入框自带的 ⌘C、⌘Z、⌘F 等仍归输入框）。

| 快捷键 | 作用 |
| --- | --- |
| ⇧⌘R | 开始记录：日程里有正在进行（或 10 分钟内开始）的会议时记录它，否则临时记录 |
| ⌥⇧⌘R | 临时记录 |
| ⇧⌘S | 结束记录 |
| ⇧⌘F | 显示或隐藏浮窗字幕（记录中） |
| ⌘↩ | 在会议笔记里插入当前时间戳 |
| ⌘1 – ⌘5 | 今天 / 日程 / 会议中 / 纪要库 / 说话人 |
| ⌘F | 搜索纪要 |
| ⌘, | 设置 |
| ⌘W | 关闭主窗口：只是隐藏，录制照常进行；点 Dock 图标或按 ⌘0 恢复 |
| ⌘M / ⌘H / ⌥⌘H | 最小化 / 隐藏 Friston / 隐藏其他 |
| ⌘Q | 退出；正在记录或还有会议在会后整理时先确认（再按一次 ⌘Q 即退出） |

快捷键只在 Friston 处于前台时生效。

## 已知限制

- 没有屏幕录制权限时，开始记录会自动降级为只录麦克风，并提示去系统设置里授权；授权后才会录窗口和系统声音。
- 系统声音需要“屏幕与系统录音”权限；授权状态可在“设置 › 系统权限”里查看和请求。开发包用固定的开发证书签名，授权在重新构建后仍然有效。
- 方位识别需要 ≥2 声道的外接麦克风（见 [说话人识别](docs/speaker-identification.md)）。
- 说话人分离已用真实会议语料（AMI）评测并调参，结果见 [说话人识别](docs/speaker-identification.md)。
