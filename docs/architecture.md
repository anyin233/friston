# 架构

```
┌───────────────────────────── Friston.app ─────────────────────────────┐
│                                                                       │
│  meeting-master (Rust, GPUI Kit)                                      │
│  ├─ mm-app      界面：今天 / 日程 / 会议中 / 纪要库 / 说话人 / 设置 + 浮窗字幕 │
│  ├─ mm-engine   JSON-RPC 客户端（stdio，换行分隔，与 Codex 相同的帧格式）    │
│  ├─ mm-index    自动索引：分块 → 差异比对 → 防抖 → 媒体切窗                 │
│  ├─ mm-store    SQLite + sqlite-vec（向量）+ FTS5（关键词）               │
│  ├─ mm-vault    Obsidian：发现库、渲染笔记、受管区块、obsidian:// URI       │
│  ├─ mm-llm      纪要：BYOK / Codex app-server / Antigravity / 本地 MLX   │
│  ├─ mm-service  无界面服务层：设置、引擎调用、字幕状态、会议流程、后台索引     │
│  ├─ mm-core     领域模型                                               │
│  └─ mm-i18n     界面语言（简体中文 / English）与 tr!/trf! 宏              │
│         │ stdio JSON-RPC                 │ stdio JSON-RPC              │
│         ▼                                ▼                            │
│  mm-meetingd (Swift)               mm-embedd (Swift)                  │
│  ScreenCaptureKit 窗口录制          EmbeddingGemma 2 (MLX, GPU)          │
│  麦克风 / 系统声音 / process tap     文字·图片·音频·视频 → 768 维           │
│  Qwen3-ASR 流式 (MLX)               按需加载、可卸载                      │
│  Sortformer + WeSpeaker (Core ML)   本地 LLM（离线纪要，JSON Schema 约束） │
│  GCC-PHAT 方位 + 说话人融合                                              │
│  EventKit 日程                                                         │
└───────────────────────────────────────────────────────────────────────┘
          │ 写 Markdown                        │ codex app-server (stdio)
          ▼                                    ▼
   Obsidian 库（用户的笔记系统）              ChatGPT 账号（无 BYOK 时）
```

## 一场会议的数据流

1. **会前**：`calendar/events` 拉取日程，识别会议链接；为会议创建 Obsidian 笔记（frontmatter、参会者 `[[双链]]`、空的受管区块）。
2. **开始记录**：`capture/start` 按预设启动：ScreenCaptureKit 只录指定窗口；麦克风（多声道时保留原始声道用于 GCC-PHAT）；系统声音。音视频由引擎直接写入 `~/Movies/Friston/`，不经过 IPC。
3. **实时字幕**：引擎推送 `caption/partial`（易变）和 `caption/final`（定稿，带说话人标签、流来源、方位），以及会话中途的 `speaker/relabel`（例如用户把“说话人 4”命名为赵磊）。GPUI 同时更新主窗口的字幕区和浮窗字幕。
4. **边开会边记笔记**：笔记编辑器即时保存到 Obsidian 文件；`⌘↩` 插入当前时间戳（链接回转写），截图直接嵌入笔记。
5. **自动索引**（全程无需操作）：
   - 信号来源：编辑器按键、Obsidian 库的 FSEvents（用户在 Obsidian 里改的也算）、新的定稿字幕、录音和录屏文件的增长。
   - `Debouncer` 在停顿 1.5 秒或连续输入满 10 秒时触发，所以会中一直打字也会定期入索引。
   - `chunk_markdown` 按标题切块，块内贪心打包，追加内容只改变最后一块。
   - `sync::plan` 按内容哈希（多重集合）比对，只嵌入新增或改动的块，删掉已消失的块。
   - 媒体按窗口嵌入：音频 30 秒，视频 20 秒、1 fps。
   - 优先级：会中笔记 > 会后编辑和转写 > 媒体回填。会议进行时嵌入任务主动让出 GPU。
   - 每个向量都带模型戳 `embeddinggemma-2@768/p1`；换模型或改了提示词时，后台自动重建索引。
6. **结束**：会后精转写和全局说话人聚类 → `mm-llm` 选择提供方生成结构化纪要（JSON Schema 约束）→ 写入笔记的 `minutes` / `actions` 受管区块 → 纪要本身也进入索引。
   - 会后整理分两段：**停止**（`capture/stop`、标记状态、写入实时转写，几秒钟）占用录制器，期间不能开始新的记录；**整理**（精转写 → 纪要）进入会后队列（`mm-app` 的 `post_queue`），一次只跑一场，后面的按停止顺序排队。所以上一场还在精转时就可以开始录下一场。排队的任务在开始运行时才读取设置，等待期间改过的设置会生效。本地精转不用录制中的引擎，而是为每场会议单独启动一个 `mm-meetingd`（`RefineWorker`），跑完即关闭、释放模型内存：同一进程里所有 MLX 调用都要排 `MLXGate`，文件级说话人分离会一次占住整段录音的时间，放在录制引擎里会让新会议的实时字幕停顿。队列只在内存里，退出 App 时未完成的会议停在“整理中”。
   - 设置里“精转方式”选 **Antigravity · Gemini** 时，会后精转写改走外部工具：录音转成 16 kHz 单声道 AAC，按 15 分钟切段，静音的音轨不发送；Antigravity CLI（`agy -p`）让 Gemini 3.8 Flash 直接听录音并输出带时间戳的逐句 JSON，说话人身份按时间重叠沿用实时分离的结果（同一个人色相不变）。如果纪要也选了 Antigravity、模型和精转相同、且录音只有一段（15 分钟内），同一次 `agy` 调用同时输出转写和纪要（schema 为 `{transcript, minutes}`）；否则精转写完成后再按纪要设置单独写纪要（长录音逐段识别，纪要需要整场内容，只能放在最后）。精转失败回退到本地 Qwen3-ASR；一并生成的纪要不可用时改为单独生成。
7. **检索**：用户的问题先转成 `search_query` 向量，再与 FTS5 的 trigram 结果融合（RRF）。结果可能是转写、笔记、截图、录音窗口或录屏窗口，点击后跳到对应的时间点。

## 服务层（`mm-service`）

界面只负责窗口和渲染，所有“做事”的代码都在 `mm-service`，可以脱离显示器和引擎测试：

| 模块 | 职责 |
| --- | --- |
| `config` | `~/Library/Application Support/Friston/config.json`（可用 `MM_DATA_DIR` 覆盖），缺键用默认值 |
| `engines` | 定位引擎（`MM_ENGINES_DIR` → App 包内 `Contents/Resources/engines/<引擎>/` → `target/engines/{Debug,Release}`），类型化调用，权限错误单独区分 |
| `live` | 字幕板（易变/定稿/撤回/改标）与说话人目录（名字和色相：同一个人在所有界面同一色相） |
| `flow` | 一场会议的各步：建笔记 → 录制预设（含已登记声纹）→ 定稿入库并写入笔记 `transcript` 区块 → 会后精转写合并 → 纪要写入 `minutes` / `actions` 区块 |
| `hub` | 从 Hugging Face（或镜像）下载模型：按提交固定版本、逐文件写 `.part` 后改名、断点续传、可取消，写到 `<数据文件夹>/models/<作者>--<模型>/`；下载完成后写 `.mm-download-complete` 标记；`find_local_snapshot` 按标记或 Hugging Face 缓存（`HF_HUB_CACHE`/`HF_HOME`/`~/.cache/huggingface/hub`）找已有副本，启动时没配嵌入模型就先找、找不到再下载（`auto_download_embedding`，默认开）；令牌存钥匙串（服务 `Friston`，账户 `huggingface`），没有时用 `HF_TOKEN` |
| `external` | 外部语音模型精转写的纯逻辑：`.caf` → 分段 AAC（`afconvert`，静音检测 −70 dBFS）、读写 16 位 WAV、把模型听到的句子映射回本场会议的说话人；调用 `agy` 的部分在 `mm_llm::antigravity` |
| `indexer` | 后台线程：Obsidian 库 FSEvents → 防抖 → 增量分块 → 按模型戳补嵌入；会议进行时减小批量，把 GPU 让给字幕 |

纪要提供方由设置“纪要生成方式”决定（`mm_llm::plan_providers`）：“自动”按 BYOK → Codex → 本地 MLX 的顺序选择；指定 API 密钥 / Codex / Antigravity 时先用指定的一项，它用不了或失败时改用自动顺序（不会重试刚失败的那一项），会后报告和通知里会说明。Codex 的模型通过 `thread/start.model` 传入（不选则用 Codex 默认），Antigravity 用 `agy --model`；两者的模型列表分别来自 `model/list` 和 `agy models`。本地模型跑在 `mm-embedd` 里（同一个 `mlx-swift-lm` 版本），用 `MLXGuidedGeneration` 把输出约束为纪要 JSON Schema，并加空白抑制，避免小模型无限输出空白。

纪要里的时间戳链接形如 `friston://open?meeting=<id>&t=<毫秒>`。App 同时注册 `friston` 和旧的 `meeting-master`，因为更名前写进笔记的链接仍用旧 scheme。

## 界面语言（i18n）

- 支持简体中文和 English。设置 › 通用 › 界面语言：跟随系统（默认，读取 macOS 首选语言，既不是中文也不是英文时用英文）、简体中文、English。保存在 `config.json` 的 `ui_language`（`"zh"` / `"en"` / 缺省）。
- 文案写在调用处，成对出现：`tr!("开始记录", "Start recording")` 返回 `&'static str`；`trf!("今天有 {n} 场会议", "{n} meetings today")` 返回 `String`，两种语言的占位符都由 `format!` 在编译期检查。只有两种语言，成对写法让两种措辞都在眼前，也不会出现运行时缺键。
- 切换时 `apply_ui_language` 同时设置 `mm_i18n` 和 gpui-component 自带文案（`rust-i18n`，`zh-CN` / `en`），然后刷新所有窗口；已经生成的提示保留原语言。
- 写进 Obsidian 笔记的标题和提示（`## 笔记` / `## Notes`、纪要小标题等）跟随界面语言；纪要正文语言仍由“纪要语言”决定。解析笔记的代码同时认中文、英文和旧的双语标题。
- 不翻译：发给模型的提示词、ASR 上下文、索引文本（改动会触发重新索引）、日志、协议字段。
- 测试默认中文；要检查英文用 `mm_i18n::with_lang(Lang::En, || …)`，它只影响当前线程，不要在测试里调用 `set_current`。

## 打包

- `scripts/dev-app.sh launch`：开发包 `target/debug/FristonDev.app`（Bundle ID `dev.friston.app.dev`），内含 Debug 引擎；每个引擎带自己的资源包和 `mlx.metallib`，放在独立目录里（两个引擎的 MLX 版本不同，资源包同名）。
- `scripts/package-app.sh`：发布包 `target/release/Friston.app`（Bundle ID `dev.friston.app`）（Release 引擎），`MM_SIGN_IDENTITY` 指定签名身份，默认 ad-hoc。
- 两者共用 `scripts/lib/bundle.sh`：Info.plist 里写麦克风、日历、屏幕录制、系统声音的用途说明和 URL scheme。

## 进程与权限

- 两个引擎作为辅助可执行文件放在 App 包内，由主进程启动。TCC 权限（麦克风、屏幕录制、日历）归属于主 App，`Info.plist` 的用途说明也写在主 App 里。
- 开发时直接 `cargo run`，权限会归到启动它的终端。
- 会后本地精转写时会多出一个临时的 `mm-meetingd` 进程（只处理 `transcribe/file`，不录制、不发通知），任务结束即 `shutdown`。
- 引擎崩溃（例如 MLX 内存不足）不会带倒界面。`mm-engine` 会让所有未完成的调用立即失败，并发出 `Closed` 事件，界面可以重启引擎。
- `shutdown` 会等所有进行中的请求都回复后再退出（有测试）。

## 目录

```
crates/          Rust 工作区（见上图）
native/
  NativeKit/     无 ML 依赖：EngineKit（JSON-RPC 服务端）、MeetingCore（GCC-PHAT、说话人融合、麦克风探测、阵列声道选择）
  MeetingEngine/ mm-meetingd（依赖 speech-swift）
  EmbedEngine/   mm-embedd（依赖 mlx-swift-lm main）
design/mockups/  界面原型（HTML/SVG），index.html 可直接打开
docs/            本文档、技术栈、说话人识别、引擎协议
scripts/         bootstrap.sh、build-engines.sh；dev-app.sh + gui_agent.py（GUI 自动化测试，见 docs/agent-gui-testing.md）
```

## 从 Meeting Master 更名为 Friston

启动时 `mm_service::legacy` 做一次性迁移，之后不再触发：

- `~/Library/Application Support/Meeting Master` 改名为 `…/Friston`；设置里指向旧文件夹内部的路径（如应用内下载的模型）一并改写。
- 没自定义录音文件夹时，`~/Movies/Meeting Master` 改名为 `~/Movies/Friston`，数据库里的 `meetings.recording_dir`、素材 `sources.uri` 和声纹片段路径随之改写（`Store::rebase_paths`）。
- 新旧文件夹同时存在时两边都不动，只提示用户。设置了 `MM_DATA_DIR` 时跳过迁移。
- 钥匙串密钥在第一次读取时从服务 `Meeting Master` 移到 `Friston`（`mm_llm::keychain`）。
- Bundle ID 从 `dev.meetingmaster.app(.dev)` 换成 `dev.friston.app(.dev)`，所以麦克风、录屏、日历权限和 Computer Use 授权要重新给一次。
