# 技术栈决策（v0.1，2026-11-05）

本文记录每项选择、理由，以及已经在本机验证过的事实。未验证的内容会明确标出“待验证”。

## 总览

| 层 | 选择 | 版本 / 锁定 | 状态 |
| --- | --- | --- | --- |
| GUI | **GPUI Kit**（`gpui-kit`，含 gpui-component） | 0.9.0（固定 GPUI `gpui-pre` 0.3.11） | 已编译并启动窗口 |
| 应用语言 | Rust（edition 2024） | rustc 1.99.1，`rust-toolchain.toml` 锁定 | 已安装 |
| 原生引擎 | Swift 6 可执行文件 × 2，通过 stdio JSON-RPC 与 Rust 通信 | Xcode 27.1 / Swift 6.4，macOS 15+ | 已编译、已冒烟测试 |
| 推理框架 | **MLX**（GPU，M5 神经加速器）+ **Core ML**（神经引擎） | mlx-swift 0.33.0 | 已编译 |
| 实时字幕 / 转写 | Qwen3-ASR（0.6B 流式 / 1.7B 会后）+ Qwen3-ForcedAligner，来自 speech-swift | speech-swift 0.0.29 | 依赖已编译，模型未下载 |
| 说话人 | Sortformer 流式分离 + WeSpeaker/CAM++ 声纹 + GCC-PHAT 方位（自研） | 同上 | 融合逻辑已实现并测试 |
| 多模态嵌入 | **EmbeddingGemma 2**（文字/图片/音频/视频，768 维，MRL） | mlx-swift-lm `main@c8dabc6` | 已实测：文字检索排序正确，录音按时间窗嵌入可用 |
| 存储 / 检索 | SQLite（rusqlite bundled）+ **sqlite-vec** + FTS5 trigram | rusqlite 0.41，sqlite-vec =0.1.9 | 已测试（KNN、中文全文） |
| 笔记系统 | Obsidian 库（直接写 Markdown）+ `obsidian://` URI；Obsidian CLI 可选 | Obsidian 1.12+ | 已实现并测试 |
| 纪要生成 | BYOK（Anthropic / OpenAI / Gemini / 兼容端点）→ **Codex app-server**（ChatGPT 登录）→ 本地 MLX（Qwen3-4B-Instruct 4bit，JSON Schema 约束） | codex-cli 0.166.0 | Codex 与本地模型已端到端实测 |
| 录制 | ScreenCaptureKit（窗口级）、AVFoundation、Core Audio process tap | 系统框架 | 窗口枚举、窗口/显示器录制、麦克风多声道与系统声音录制已实现 |
| 日程 | EventKit | 系统框架 | 已实现读取 |

## 关键决定与理由

### 1. GUI：GPUI Kit 0.9（单一依赖）
- 你指定的 `longbridge/gpui-kit` 已发布到 crates.io，`gpui-kit = "0.9"` 一个依赖就带来 GPUI、gpui-base、gpui-component 和 Lucide 图标；不再需要 git 依赖 Zed 仓库。
- 需要用到的组件都有：Sidebar、TitleBar、Dock/Resizable、List/Table（虚拟化）、Input（编辑器）、Markdown、Tabs、Dialog、Setting，另有 `speech` feature（本项目不用它做识别）。
- 浮窗字幕用 GPUI 的第二个无边框、置顶、可穿透窗口实现。

### 2. 推理：Swift 原生引擎，而不是 Rust 内嵌 MLX 或 Python
考虑过的方案：

| 方案 | 结论 |
| --- | --- |
| Rust 内嵌 `mlx-rs` 0.34 | 绑定在活跃维护，但 EmbeddingGemma 2、Qwen3-ASR、Sortformer、WeSpeaker 都要用 Rust 重写，工作量和风险都太大 |
| 打包 Python + mlx-audio/mlx-vlm | 模型最全，但你明确不想依赖 Python 运行时 |
| **Swift 引擎（选用）** | Apple 官方 `mlx-swift-lm` 已有 `EmbeddingGemma2Embedding`；`speech-swift` 有 Qwen3-ASR、Sortformer 流式、WeSpeaker、Silero VAD、回声消除；ScreenCaptureKit / EventKit / AVFoundation 在 Swift 里是一等公民 |

**拆成两个进程**（`mm-meetingd` 和 `mm-embedd`）有两个原因：
1. 依赖冲突（已核实）：speech-swift 0.0.29 把 `mlx-swift-lm` 锁在 **exact 3.31.4**，而 EmbeddingGemma 2 目前只在 `mlx-swift-lm` 的 **main 分支**（3.31.4 和 3.32.3 两个 tag 里都没有）。同一个 SwiftPM 依赖图解析不了。
2. 实时性：会议中字幕和说话人识别不能被后台索引抢占。分进程后，嵌入引擎可以降低优先级，或者随时卸载模型。

### 3. 多模态嵌入：EmbeddingGemma 2
- 2026-10-06 发布，Apache 2.0。基于 Gemma 4，共 740M 参数（文本 270M + 视觉 170M + 音频 300M，可按模态加载），8K 上下文，输出 768 维，MRL 可截到 512/256/128。
- 每种模态的 token 消耗：音频 25 token/秒，视频帧 140 token，图片 280 token。所以音频按 30 秒一个窗口，视频按 20 秒、1 fps 一个窗口（见 `crates/mm-index/src/media.rs`）。
- Swift 实现：`MultimodalEmbeddingFactory.load(modelDirectory:tokenizerLoader:)` 加 `embed(_:task:)`。模型自己负责任务提示词，所以 Rust 侧**不能**再拼 `title: … | text:`，标题单独传（这个坑已修正）。
- 权重：`mlx-community/embeddinggemma-2-bf16`（官方 MLX 转换），约 1.6 GB。mlx-vlm 的测试表明 8-bit 几乎无损，但普通 4-bit 达不到精度要求，所以默认用 bf16。

### 4. 语音：Qwen3-ASR + Sortformer + 声纹 + 方位
- Qwen3-ASR 支持中、英、日、韩以及 22 种中文方言，有流式模式。实时字幕用 0.6B，会后精转写用 1.7B，再用 ForcedAligner 得到逐字时间戳。
- 说话人识别的设计见 [speaker-identification.md](speaker-identification.md)。
- 备选：Apple SpeechAnalyzer（系统自带，零下载），可以作为实时字幕的降级方案。

### 5. 纪要生成：BYOK → 本机 ChatGPT（Codex）→ 本地 MLX
- **ChatGPT 桌面 App 没有公开的本地 API**（Meetings 插件把音频传到云端处理，也没有本地接口）。官方支持的本地集成方式是 `codex app-server`：stdio 上的 JSON-RPC，账号是用户已经登录的 ChatGPT 账号。
- 本机 codex-cli 0.166.0 生成的 schema 已存放在 `crates/mm-llm/schema/`，测试会校验我们发出的每个字段：
  - `UserInput` 支持 `text`、`localImage`、`localAudio`，所以会中截图和存疑的录音片段都可以直接交给模型。
  - `turn/start.outputSchema` 约束最终输出为我们的纪要 JSON。
  - 线程设为 `ephemeral` + `read-only` sandbox + `approvalPolicy: never`，不碰文件系统。
- 注意：OpenAI 文档写明 app-server 认证**不得**用于商业或托管服务。本项目是个人本地应用，可以使用；如果将来要分发，需要改用 “Sign in with ChatGPT”。

### 6. 存储：SQLite + sqlite-vec + FTS5
- 单个数据库文件，可备份、可查看。向量表 `vec0(float[768], cosine)`；FTS5 用 trigram 分词，中文不需要分词器（测试已覆盖“预算评审”这类子串）。
- `sqlite-vec` 锁在 **=0.1.9**：0.1.10-alpha.4 发布的源码缺少 `sqlite-vec-diskann.c`，无法编译（已实测）。
- 没有选 LanceDB：Arrow/DataFusion 依赖太重，个人会议库的规模（十万级片段）用暴力 KNN 已经足够。

### 7. Obsidian
- 直接把 Markdown 写进库，Obsidian 没开也能写；用 `obsidian://open` 打开笔记。Obsidian CLI（1.12+）要求 Obsidian 正在运行，所以只作可选增强。
- 生成的内容只写在 `<!-- mm:minutes:start/end -->` 这样的受管区块里，用户在 Obsidian 里写的其他内容永远不会被覆盖（有测试）。
- 写入时先写临时文件再 rename，避免 Obsidian 读到写了一半的文件。

## 本次安装 / 变更的组件

| 组件 | 操作 |
| --- | --- |
| Rust | `rustup update stable`：1.92.0 → 1.99.1 |
| Metal Toolchain | `xcodebuild -downloadComponent MetalToolchain`（MLX 和 GPUI 着色器需要；之前未安装） |
| Cargo 依赖 | gpui-kit 0.9、rusqlite（bundled SQLite）、sqlite-vec 0.1.9 等，见 `Cargo.lock` |
| SwiftPM 依赖 | mlx-swift 0.33.0、mlx-swift-lm main、swift-transformers 1.4.0、swift-huggingface 0.13.0、speech-swift 0.0.29 及其传递依赖，见各包的 `Package.resolved` |

已下载到 Hugging Face 缓存（`~/.cache/huggingface/hub`）：`mlx-community/embeddinggemma-2-bf16`（1.5 GB）、`mlx-community/Qwen3-4B-Instruct-2507-4bit`（离线纪要，约 2.3 GB）。在“设置”里选择对应的 snapshot 文件夹即可加载。

## 待验证

- DJI Mic Mini（2 TX + 1 RX）接 Mac 时是否输出 2 个独立声道，以及两路之间的时延是否稳定（抖动 < 0.3 样本），清单见 spatial-feasibility.md 2.4 节。
- Qwen3-ASR 0.6B 在 M5 Pro 上的流式延迟和中文准确率，需要下载模型后实测。
- 两个 MLX 进程同时运行时的显存占用。
