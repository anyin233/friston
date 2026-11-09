# 技术栈决策（v0.1，2026-11-05）

本文记录每项选择、理由，以及已经在本机验证过的事实。未验证的内容会明确标出“待验证”。

## 总览

| 层 | 选择 | 版本 / 锁定 | 状态 |
| --- | --- | --- | --- |
| GUI | **GPUI Kit**（`gpui-kit`，含 gpui-component） | 0.9.0（固定 GPUI `gpui-pre` 0.3.11） | 已编译并启动窗口 |
| 应用语言 | Rust（edition 2024） | rustc 1.99.1，`rust-toolchain.toml` 锁定 | 已安装 |
| 原生引擎 | Swift 6 可执行文件 × 2，通过 stdio JSON-RPC 与 Rust 通信 | Xcode 27.1 / Swift 6.4，macOS 15+ | 已编译、已冒烟测试 |
| 推理框架 | **MLX**（GPU，M5 神经加速器）+ **Core ML**（神经引擎） | mlx-swift 0.33.0 | 已编译 |
| 实时字幕 / 转写 | Qwen3-ASR（1.7B 4bit 实时 / 1.7B 8bit 会后）+ Qwen3-ForcedAligner，来自 speech-swift | speech-swift 0.0.29 | 依赖已编译，模型未下载 |
| 实时字幕草稿层 | 流式 Zipformer 中文 int8（sherpa-onnx C API，ONNX Runtime，CPU） | sherpa-onnx 2.0.1 静态库，`scripts/fetch-sherpa-onnx.sh` 按 SHA-256 下载，打包为 `CSherpaOnnx.xcframework`（不入库） | 已集成；GPU 跟不上时自动启用 |
| 说话人 | Sortformer 流式分离 + WeSpeaker/CAM++ 声纹 + GCC-PHAT 方位（自研） | 同上 | 融合逻辑已实现并测试 |
| 多模态嵌入 | **EmbeddingGemma 2**（文字/图片/音频/视频，768 维，MRL） | mlx-swift-lm `main@c8dabc6` | 已实测：文字检索排序正确，录音按时间窗嵌入可用 |
| 存储 / 检索 | SQLite（rusqlite bundled）+ **sqlite-vec** + FTS5 trigram | rusqlite 0.41，sqlite-vec =0.1.9 | 已测试（KNN、中文全文） |
| 笔记系统 | Obsidian 库（直接写 Markdown）+ `obsidian://` URI；Obsidian CLI 可选 | Obsidian 1.12+ | 已实现并测试 |
| 纪要生成 | 自动：BYOK（Anthropic / OpenAI / Gemini / 兼容端点）→ **Codex app-server**（ChatGPT 登录）→ 本地 MLX（Qwen3-4B-Instruct 4bit，JSON Schema 约束）；也可指定 BYOK / Codex / **Antigravity CLI**，Codex 与 Antigravity 可选模型 | codex-cli 0.166.0 | Codex 与本地模型已端到端实测 |
| 录制 | ScreenCaptureKit（窗口级）、AVFoundation、Core Audio process tap | 系统框架 | 窗口枚举、窗口/显示器录制、麦克风多声道与系统声音录制已实现 |
| 日程 | EventKit | 系统框架 | 已实现读取 |

## 关键决定与理由

### 1. GUI：GPUI Kit 0.9（单一依赖）
- 你指定的 `longbridge/gpui-kit` 已发布到 crates.io，`gpui-kit = "0.9"` 一个依赖就带来 GPUI、gpui-base、gpui-component 和 Lucide 图标；不再需要 git 依赖 Zed 仓库。
- 需要用到的组件都有：Sidebar、TitleBar、Dock/Resizable、List/Table（虚拟化）、Input（编辑器）、Markdown、Tabs、Dialog、Setting，另有 `speech` feature（本项目不用它做识别）。
- 浮窗字幕用 GPUI 的第二个无边框窗口（`WindowKind::PopUp`，即非激活的 `NSPanel`）实现；再通过原生句柄设为跨所有桌面、可浮在全屏应用之上、应用失去焦点时不隐藏（`crates/mm-app/src/floating.rs`）。

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
- Qwen3-ASR 支持中、英、日、韩以及 22 种中文方言，有流式模式。实时字幕用 1.7B 4bit（2026-11-06 从 0.6B 换来，字错误率 4.78% → 3.63%，见 [asr-model-survey.md](asr-model-survey.md) 第 5 节），会后精转写用 1.7B 8bit，再用 ForcedAligner 得到逐字时间戳。
- 说话人识别的设计见 [speaker-identification.md](speaker-identification.md)。
- 备选：Apple SpeechAnalyzer（系统自带，零下载），可以作为实时字幕的降级方案。
- **草稿层（2026-11-08）**：sherpa-onnx 的流式 Zipformer 中文 int8（167 MB，320 ms 一块，CPU）。它不走 MLX，GPU 被占满时仍能边说边出字；定稿始终由 Qwen3-ASR 给出。M5 Pro 上 Qwen3 前缀回退的临时字幕更快，所以默认只在 Qwen3 刷新间隔 ≥ 0.6 s 时切换（`DraftPolicy`），`MM_DRAFT=1/0` 可强制开/关。sherpa-onnx 没有 SwiftPM 包，官方只发布静态库，所以由脚本合并成一个静态库加 module map，用 `binaryTarget(path:)` 链接；只有中文字幕启用。选型过程见 [asr-model-survey.md](asr-model-survey.md) 第 9 节。

### 5. 纪要生成：BYOK → 本机 ChatGPT（Codex）→ 本地 MLX
- **ChatGPT 桌面 App 没有公开的本地 API**（Meetings 插件把音频传到云端处理，也没有本地接口）。官方支持的本地集成方式是 `codex app-server`：stdio 上的 JSON-RPC，账号是用户已经登录的 ChatGPT 账号。
- 本机 codex-cli 0.166.0 生成的 schema 已存放在 `crates/mm-llm/schema/`，测试会校验我们发出的每个字段：
  - `UserInput` 支持 `text`、`localImage`、`localAudio`，所以会中截图和存疑的录音片段都可以直接交给模型。
  - `turn/start.outputSchema` 约束最终输出为我们的纪要 JSON。
  - 线程设为 `ephemeral` + `read-only` sandbox + `approvalPolicy: never`，不碰文件系统。
- 模型选择（2026-11-09）：设置里的列表来自 `model/list`（`includeHidden: false`，按 `nextCursor` 翻页），选中的 id 放进 `thread/start.model`；选“Codex 默认模型”时不传该字段。codex-cli 0.167.0 实测列出 `gpt-6.1-sol`（默认）、`gpt-6.1-terra`、`gpt-6-luna` 等 8 个模型，都只支持 `text`、`image` 输入。
- 也可以指定 **Antigravity**（`agy -p`）写纪要：只给文字，工作目录为空，用 `--json-schema` 约束输出。模型列表来自 `agy models`（Gemini、Claude、GPT-OSS 等），实测 `claude-opus-5-5-low` 约 8 秒写出符合 schema 的纪要。复跑：`cargo test -p mm-llm --test antigravity_minutes_live -- --ignored --nocapture`。
- 注意：OpenAI 文档写明 app-server 认证**不得**用于商业或托管服务。本项目是个人本地应用，可以使用；如果将来要分发，需要改用 “Sign in with ChatGPT”。

### 5b. 会后精转写的外部工具：Antigravity（Gemini 3.8 Flash）（2026-11-08）
- 选项在设置“精转方式”里，默认仍是本地 Qwen3-ASR。选 Antigravity 后录音会上传到 Google。
- 调用方式：`agy -p … --model gemini-3.8-flash-medium --output-format json --json-schema … --settings '{"permissions":{"allow":["read_file(<工作目录>/**)"]}}'`。工作目录里只有准备好的录音；无头模式下其他需要权限的工具（运行命令、写文件）自动拒绝，不传 `--dangerously-skip-permissions`。
- agy 可能以退出码 0 返回 `status: "ERROR"`（服务端 500 后报 "The stream was interrupted"），所以必须检查 `status`；失败时重试一次。噪声多的录音上，Flash 偶尔在 JSON 之后继续生成直到被切断（3 句话输出 1.7 万 token），此时已有的结构化结果是完整的，只在没有干净结果时使用，并在会后报告里提示可能不完整。
- **Codex 不能直接听音频**：`model/list` 里 `gpt-6.1-terra`、`gpt-6.1-sol` 等都只声明 `text`、`image` 输入；把录音作为 `localAudio` 发过去，两者都回复“无法读取音频”，terra 还会自己去找本机的 whisper 来跑。所以 Codex 不做这条路，只保留原来的纪要提供方角色。
- 实测（M5 Pro，`gemini-3.8-flash-medium`）：

| 录音 | 结果 |
| --- | --- |
| 合成两人会议 10 分 25 秒，64 句 | 52 秒完成；64 句全对，时间戳误差约 1 秒，说话人交替正确，末尾的金额和待办都写进纪要 |
| 真实录音 2 分钟，麦克风大部分是键盘声，系统声音为数字静音 | 只转出 98–110 秒的几句话，正确报告系统声音没有人声；部分运行被服务端中断（见上） |
| 同上，改用 `gemini-3.8-flash-low` | **编造了一整场“模型服务优化”会议**，还带人名。所以不提供 low，并且静音音轨不发送 |

- 精转模型只列能听音频的 Gemini 模型，并去掉 `-low`。纪要也选 Antigravity 且模型相同、录音只有一段时，一次调用同时返回转写和纪要（合成的两人短会议实测：两句全对，纪要的决定、金额和待办人都正确）；否则精转后再单独写纪要。
- 复跑：`cargo test -p mm-service --test antigravity_live -- --ignored --nocapture`（`MM_AGY_RECORDING=<录音>` 换成真实录音）。

### 6. 存储：SQLite + sqlite-vec + FTS5
- 单个数据库文件，可备份、可查看。向量表 `vec0(float[768], cosine)`；FTS5 用 trigram 分词，中文不需要分词器（测试已覆盖“预算评审”这类子串）。
- `sqlite-vec` 锁在 **=0.1.9**：0.1.10-alpha.4 发布的源码缺少 `sqlite-vec-diskann.c`，无法编译（已实测）。
- 没有选 LanceDB：Arrow/DataFusion 依赖太重，个人会议库的规模（十万级片段）用暴力 KNN 已经足够。

### 7. Obsidian
- 直接把 Markdown 写进库，Obsidian 没开也能写；用 `obsidian://open` 打开笔记。Obsidian CLI（1.12+）要求 Obsidian 正在运行，所以只作可选增强。
- 生成的内容只写在 `<!-- mm:minutes:start/end -->` 这样的受管区块里，用户在 Obsidian 里写的其他内容永远不会被覆盖（有测试）。
- 写入时先写临时文件再 rename，避免 Obsidian 读到写了一半的文件。

### 8. 界面语言：自写 `mm-i18n`，系统语言用 `sys-locale`
- 只做简体中文和英文，所以用调用处成对的 `tr!` / `trf!` 宏，而不是 `rust-i18n` 的 YAML 键（gpui-component 内部用它，键缺失只能在运行时发现）。gpui-component 自带文案仍通过它的 `set_locale` 切换。
- `sys-locale` 0.3 读取 macOS 首选语言（CFLocale），已经是 GPUI 的间接依赖，没有新增下载。

## 本次安装 / 变更的组件

| 组件 | 操作 |
| --- | --- |
| Rust | `rustup update stable`：1.92.0 → 1.99.1 |
| Metal Toolchain | `xcodebuild -downloadComponent MetalToolchain`（MLX 和 GPUI 着色器需要；之前未安装） |
| Cargo 依赖 | gpui-kit 0.9、rusqlite（bundled SQLite）、sqlite-vec 0.1.9 等，见 `Cargo.lock` |
| SwiftPM 依赖 | mlx-swift 0.33.0、mlx-swift-lm main、swift-transformers 1.4.0、swift-huggingface 0.13.0、speech-swift 0.0.29 及其传递依赖，见各包的 `Package.resolved` |

已下载到 Hugging Face 缓存（`~/.cache/huggingface/hub`）：`mlx-community/embeddinggemma-2-bf16`（1.5 GB）、`mlx-community/Qwen3-4B-Instruct-2507-4bit`（离线纪要，约 2.3 GB）。在“设置”里选择对应的 snapshot 文件夹即可加载；嵌入模型也可以在设置页直接从 Hugging Face 下载（`mm-service::hub`，用 `ureq`，默认地址即上面的 bf16 仓库）；没配嵌入模型时，App 启动会自动使用缓存里的这份，没有就自动下载。

## 实测（2026-11-05，M5 Pro）

| 项目 | 结果 | 复跑 |
| --- | --- | --- |
| 实时字幕中文准确率（Qwen3-ASR 0.6B 4bit，合成语音 58 秒、8 句） | 字错误率 2.4% | 测试脚本见会话记录：`caption/simulateFile` 以实时速度回放，与参考文本按字比对 |
| 实时字幕跟随（2026-11-07，自适应刷新 + 停顿处切句，合成语音 68 秒 12 句含一段 16 秒独白） | partial 间隔中位 0.33 s（原固定 1 s）；句末到屏幕上出现最后几个字 p90 0.07 s（原 1.2 s）；首条字幕 p90 1.1 s（原 1.7 s）；定稿延迟中位 1.15 s，最大 3.4 s（原 8.3 s，长独白在 12 s 处切在词中间）；字错误率 0 | `scripts/eval-captions.py` |
| 定稿字幕延迟（从一句话结束到字幕定稿） | 中位数 0.77 秒，p90 0.88 秒（AISHELL-1 拼接 7 分钟）。2026-11-06 之前的 3.6 秒偏高：`caption/simulateFile` 每 100 ms 固定睡眠，回放比实时慢约 4%，延迟随时长累积；现已按时钟定步 | 同上 |
| 第一条易变字幕（从真实开口算起） | 中位数 1.15 秒，p90 4.45 秒（同上，已修正回放定步） | 同上 |
| 实时字幕 0.6B 对 1.7B（AISHELL-1 测试集 70 句拼成 7 分钟） | 字错误率 0.6B 4bit 4.78%、1.7B 4bit 3.63%、1.7B 8bit 3.63%；定稿延迟 0.77 / 0.81 / 0.84 秒；内存峰值 0.82 / 1.66 / 2.49 GB | [asr-model-survey.md](asr-model-survey.md) 第 5 节 |
| 会后精转写速度（Qwen3-ASR 1.7B 8bit） | 约 0.2–0.3 倍实时（50 秒录音 14–15 秒） | `transcribe/file` |
| 说话人分离 | 见 [speaker-identification.md](speaker-identification.md) 的 AMI 评测 | `scripts/eval-diarization.py` |
| 内存：`mm-meetingd` 实时字幕模型（1.7B 4bit + VAD + WeSpeaker + Sortformer） | 进程峰值约 1.7 GB（换模型前 0.6B 为 0.8 GB） | `ps` 采样 RSS |
| 内存：`mm-embedd` 加载 EmbeddingGemma 2 | 约 0.75 GB；再加载本地 Qwen3-4B 纪要模型后约 2.9 GB | `footprint` |
| 会后 1.7B 模型 | 用完即释放；MLX 缓存会占住约 1 GB，现在转写结束后主动清空（MLX 活跃内存回到 0.7 GB） | 引擎日志 `MLX active … cache … → 0 MB` |

两个 MLX 进程同时运行时合计约 2–4 GB，取决于是否加载本地纪要模型，对 32 GB 以上的机器没有压力。

## 可选验证（有设备时再做，2026-11-05 决定）

- DJI Mic Mini（2 TX + 1 RX）接 Mac 时是否输出 2 个独立声道，以及两路之间的时延是否稳定（抖动 < 0.3 样本），清单见 spatial-feasibility.md 2.4 节。需要外接硬件，本机目前没有接入。
- XREAL One Pro 双麦的方位判型阈值校准，同样需要接入设备录一段真实会议。
