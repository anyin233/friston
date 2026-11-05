# 架构

```
┌───────────────────────── Meeting Master.app ──────────────────────────┐
│                                                                       │
│  meeting-master (Rust, GPUI Kit)                                      │
│  ├─ mm-app      界面：今天 / 日程 / 会议中 / 纪要库 / 说话人 / 设置 + 浮窗字幕 │
│  ├─ mm-engine   JSON-RPC 客户端（stdio，换行分隔，与 Codex 相同的帧格式）    │
│  ├─ mm-index    自动索引：分块 → 差异比对 → 防抖 → 媒体切窗                 │
│  ├─ mm-store    SQLite + sqlite-vec（向量）+ FTS5（关键词）               │
│  ├─ mm-vault    Obsidian：发现库、渲染笔记、受管区块、obsidian:// URI       │
│  ├─ mm-llm      纪要：BYOK / Codex app-server / 本地 MLX                 │
│  └─ mm-core     领域模型                                               │
│         │ stdio JSON-RPC                 │ stdio JSON-RPC              │
│         ▼                                ▼                            │
│  mm-meetingd (Swift)               mm-embedd (Swift)                  │
│  ScreenCaptureKit 窗口录制          EmbeddingGemma 2 (MLX, GPU)          │
│  麦克风 / 系统声音 / process tap     文字·图片·音频·视频 → 768 维           │
│  Qwen3-ASR 流式 (MLX)               按需加载、可卸载                      │
│  Sortformer + WeSpeaker (Core ML)                                     │
│  GCC-PHAT 方位 + 说话人融合                                              │
│  EventKit 日程                                                         │
└───────────────────────────────────────────────────────────────────────┘
          │ 写 Markdown                        │ codex app-server (stdio)
          ▼                                    ▼
   Obsidian 库（用户的笔记系统）              ChatGPT 账号（无 BYOK 时）
```

## 一场会议的数据流

1. **会前**：`calendar/events` 拉取日程，识别会议链接；为会议创建 Obsidian 笔记（frontmatter、参会者 `[[双链]]`、空的受管区块）。
2. **开始记录**：`capture/start` 按预设启动：ScreenCaptureKit 只录指定窗口；麦克风（多声道时保留原始声道用于 GCC-PHAT）；系统声音。音视频由引擎直接写入 `~/Movies/Meeting Master/`，不经过 IPC。
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
7. **检索**：用户的问题先转成 `search_query` 向量，再与 FTS5 的 trigram 结果融合（RRF）。结果可能是转写、笔记、截图、录音窗口或录屏窗口，点击后跳到对应的时间点。

## 进程与权限

- 两个引擎作为辅助可执行文件放在 App 包内，由主进程启动。TCC 权限（麦克风、屏幕录制、日历）归属于主 App，`Info.plist` 的用途说明也写在主 App 里。
- 开发时直接 `cargo run`，权限会归到启动它的终端。
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
