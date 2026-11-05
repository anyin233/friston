# 引擎协议（v0）

传输：子进程的 stdin/stdout，每行一个 JSON-RPC 2.0 对象；stderr 只用来输出日志。
方法名常量有两份，必须同步修改：`crates/mm-engine/src/methods.rs` 和 `native/NativeKit/Sources/EngineKit/Methods.swift`。

## 通用

| 方法 | 参数 | 结果 |
| --- | --- | --- |
| `initialize` | – | `{name, version, capabilities}` |
| `shutdown` | – | `null`；所有进行中的请求都回复后才退出 |

错误码：`-32601` 方法不存在 · `-32602` 参数错误（包括解码失败，并指出出错的字段） · `-32004` 尚未实现 · `-32010` 屏幕录制权限 · `-32011` 日历权限 · `-32020` 嵌入模型未加载 · `-32000` 其他错误（附上下文）。

## mm-meetingd

| 方法 / 通知 | 状态 | 说明 |
| --- | --- | --- |
| `audio/probeInputs` | ✅ | 输入设备列表 `{uid,name,channels,sampleRate,isVirtual}` |
| `capture/listSources` | ✅ | ScreenCaptureKit 可录制的窗口与显示器 |
| `calendar/events` `{from,to}` | ✅ | ISO-8601 时间范围；返回事件和识别出的会议链接 |
| `capture/start` / `capture/stop` | ⏳ | 录制预设：窗口 ID、麦克风 UID、是否录系统声音、字幕语言 |
| `voiceprint/enroll` / `voiceprint/fromClip` | ⏳ | 返回声纹向量及质量指标 |
| `transcribe/file` | ⏳ | 会后精转写 |
| `caption/partial` · `caption/final` · `speaker/relabel` | ⏳ 通知 | 字段对应 `mm_core::CaptionEvent` |
| `spatial/cue` · `audio/levels` · `capture/state` | ⏳ 通知 | 方位、电平、录制状态 |

## mm-embedd

| 方法 | 状态 | 说明 |
| --- | --- | --- |
| `model/load` `{directory}` | ✅ | 加载本地 EmbeddingGemma 2 检查点 |
| `model/status` / `model/unload` | ✅ | 卸载后显存还给实时字幕 |
| `embed/batch` `{items:[{text,title,images,video,audio}], task, dimensions}` | ✅ | `task`：`document`（默认）、`search_query`、`clustering`……；`dimensions`：768/512/256/128 |
