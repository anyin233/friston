# 引擎协议（v0）

传输：子进程的 stdin/stdout，每行一个 JSON-RPC 2.0 对象；stderr 只用来输出日志。
方法名常量有两份，必须同步修改：`crates/mm-engine/src/methods.rs` 和 `native/NativeKit/Sources/EngineKit/Methods.swift`。

## 通用

| 方法 | 参数 | 结果 |
| --- | --- | --- |
| `initialize` | – | `{name, version, capabilities}` |
| `shutdown` | – | `null`；所有进行中的请求都回复后才退出 |

错误码：`-32601` 方法不存在 · `-32602` 参数错误（包括解码失败，并指出出错的字段） · `-32004` 尚未实现 · `-32010` 屏幕录制权限 · `-32011` 日历权限 · `-32012` 麦克风权限 · `-32020` 嵌入模型未加载 · `-32000` 其他错误（附上下文）。

## mm-meetingd

| 方法 / 通知 | 状态 | 说明 |
| --- | --- | --- |
| `audio/probeInputs` | ✅ | 输入设备列表 `{uid,name,channels,sampleRate,isVirtual}` |
| `capture/listSources` | ✅ | ScreenCaptureKit 可录制的窗口与显示器 |
| `calendar/events` `{from,to}` | ✅ | ISO-8601 时间范围；返回事件和识别出的会议链接 |
| `capture/start` | ✅ | 见下方“录制”；返回 `{sessionId,directory,files,microphoneChannels,startedAt}` |
| `capture/stop` | ✅ | 返回 `{sessionId,durationMs,files}`，`files` 只列出真正写入数据的文件 |
| `voiceprint/enroll` / `voiceprint/fromClip` | ⏳ | 返回声纹向量及质量指标 |
| `transcribe/file` | ⏳ | 会后精转写 |
| `caption/partial` · `caption/final` · `speaker/relabel` | ⏳ 通知 | 字段对应 `mm_core::CaptionEvent` |
| `audio/levels` · `capture/state` | ✅ 通知 | 电平：`{sessionId,source,rmsDb[],peakDb[]}`，每路最多 100 ms 一次；状态：`{sessionId,state,error?}` |
| `spatial/cue` | ⏳ 通知 | 方位 |

### 录制（`capture/start`）

参数（键名与 Swift 一致，Rust 端是 `mm_core::CapturePreset`）：

| 字段 | 说明 |
| --- | --- |
| `title` | 会议标题，用于文件夹名 `yyyy-MM-dd HHmm 标题`（`/`、`:` 替换为 `-`，缺省为“会议”） |
| `windowID` / `displayID` | 二选一；录指定窗口或整个显示器，HEVC 写入 `screen.mov` |
| `microphoneUID` | 来自 `audio/probeInputs`；保留全部硬件声道，Float32 写入 `mic.caf` |
| `systemAudio` | 录系统声音（不含本进程），48 kHz 立体声写入 `system.caf` |
| `captionLanguage` | 字幕语言提示，留给流式 ASR |
| `fps` | 1–60，默认 15 |
| `outputDirectory` | 覆盖默认根目录 `~/Movies/Meeting Master` |

同一时间只允许一场录制：录制中再次 `capture/start` 返回 `-32000`。状态顺序为 `starting → recording → stopping → stopped`，任一步出错为 `failed`，之后可以重新开始。`shutdown` 会先停止录制并收尾文件。

## mm-embedd

| 方法 | 状态 | 说明 |
| --- | --- | --- |
| `model/load` `{directory}` | ✅ | 加载本地 EmbeddingGemma 2 检查点 |
| `model/status` / `model/unload` | ✅ | 卸载后显存还给实时字幕 |
| `embed/batch` `{items:[{text,title,images,video,audio}], task, dimensions}` | ✅ | `task`：`document`（默认）、`search_query`、`clustering`……；`dimensions`：768/512/256/128 |
