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
| `voiceprint/enroll` / `voiceprint/fromClip` | ✅ | 见下方“声纹”；返回声纹向量及质量指标 |
| `transcribe/file` | ✅ | 见下方“整段转写”；会后精转写（Qwen3-ASR 1.7B + Pyannote 分离） |
| `caption/partial` · `caption/final` · `speaker/relabel` | ✅ 通知 | 见下方“实时字幕” |
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
| `captionLanguage` | 字幕语言提示（BCP-47，如 `zh`、`en-US`）；缺省、`auto` 或模型不支持的语言都交给模型自动识别 |
| `fps` | 1–60，默认 15 |
| `outputDirectory` | 覆盖默认根目录 `~/Movies/Meeting Master` |
| `voiceprints` | 可选，预录声纹 `[{personId, model, vector:[Float]}]`；`model` 与 `initialize` 返回的 `capabilities.speakerEmbeddingModel` 不同的向量会被忽略 |

`initialize` 的 `capabilities.models` 列出实际使用的模型：`liveASR`、`offlineASR`、`vad`、`speakerEmbedding`、`segmentation`。

同一时间只允许一场录制：录制中再次 `capture/start` 返回 `-32000`。状态顺序为 `starting → recording → stopping → stopped`，任一步出错为 `failed`，之后可以重新开始。`shutdown` 会先停止录制并收尾文件。

### 实时字幕

录制开始后，引擎在后台加载 Qwen3-ASR 0.6B、Silero VAD 和 WeSpeaker（首次使用时由 speech-swift 下载到 `~/Library/Caches/qwen3-speech/`）。模型加载失败时只在 stderr 记日志，录制照常进行，只是没有字幕。麦克风和系统声音各自切分、各自识别，流来源是说话人的硬分区。

- 断句：Silero VAD，停顿 ≥ 0.5 s 结束一句；连续讲话超过 12 s 强制断句。
- 说话过程中约每 1 s 发一次 `caption/partial`；处理落后于实时时会跳过中间的 partial，只保证 final。
- 说话人：每句 final 计算 WeSpeaker 声纹，交给 `SpeakerFusion` 与预录声纹和本场的匿名簇比对；麦克风有 ≥ 2 个真实声道时再加上方位线索。短于 1.5 s 的句子只和已有的人比对、不新建说话人，比对不上或不足 0.4 s 时标为 `unknown`。
- 时间都是相对录制开始的毫秒数；`utteranceId` 在一场录制内跨两路唯一。

| 通知 | 参数 |
| --- | --- |
| `caption/partial` | `{sessionId, utteranceId, origin, startMs, text, speaker}`，同一 `utteranceId` 的新 partial 替换旧的 |
| `caption/final` | `{sessionId, utteranceId, origin, startMs, endMs, text, language?, speaker, spatial?}`，收到后撤掉该句的 partial |
| `speaker/relabel` | `{sessionId, from, to}`：本场所有标为 `from` 的字幕改标为 `to`（匿名簇被认出是预录的人，或两个簇合并） |

- `origin`：`"microphone"` 或 `"system"`。
- `speaker`（与 `mm_core::SpeakerLabel` 相同，字段名用 snake_case）：`{"kind":"cluster","index":N}`、`{"kind":"known","person_id":"…","score":0.8}` 或 `{"kind":"unknown"}`。
- `language`：设置了 `captionLanguage` 时就是它的主语言码，否则按文字的书写系统猜（`zh`/`ja`/`ko`/`en`），猜不出时省略。
- `spatial`（与 `mm_core::SpatialCue` 相同）：`{azimuth_deg?, tdoa_samples, confidence}`，只在麦克风有方位线索时出现；`tdoa_samples` 是这句话里两声道时间差的中位数（麦克风采样率下的样本数），实时阶段不知道阵列几何，所以没有 `azimuth_deg`。
- 某句已经发过 partial、但最终识别为空时，仍会发一条 `text` 为空的 final，好让界面撤掉 partial。

### 声纹

| 方法 | 参数 |
| --- | --- |
| `voiceprint/fromClip` | `{path, startMs?, endMs?}`：从任意音频文件截取一段，范围缺省为整个文件 |
| `voiceprint/enroll` | `{microphoneUID?, seconds?}`：直接从麦克风录 `seconds` 秒（3–120，默认 15），缺省用系统默认输入 |

两者返回相同的结果：`{model, vector:[Float], speechMs, quality:{singleSpeaker, snrDb?}}`。

- 先用 VAD 去掉静音，再对全部语音算一个 L2 归一化的 WeSpeaker 向量（256 维）；`model` 是要原样保存的模型名。
- 有效语音不足 1000 ms 时返回 `-32000`；文件不存在或范围为空时返回 `-32602`。
- `singleSpeaker`：语音 ≥ 6 s 时按 3 s 分窗分别计算向量，任一窗口与整体的余弦低于 0.5 就判为混入了别人；更短时默认 `true`。
- `snrDb`：语音段与非语音段的能量比；片段里几乎没有非语音部分时省略。

### 整段转写（`transcribe/file`）

参数 `{path, language?, voiceprints?}`，返回 `{language?, segments:[{startMs, endMs, text, speaker}]}`。

- 先用 Pyannote 分离说话人（附 Silero 预过滤），同一人间隔 ≤ 0.8 s 的片段合并，超过 25 s 的切开，再逐段用 Qwen3-ASR 1.7B 识别。分离模型不可用时退回只用 VAD 分段，`speaker` 为 `unknown`。
- 1.7B 优先使用 Hugging Face 缓存里的 `mlx-community/Qwen3-ASR-1.7B-8bit`，没有时由 speech-swift 下载 `aufklarer/Qwen3-ASR-1.7B-MLX-8bit`。模型只在请求期间驻留内存。
- `speaker` 是本文件内的 `cluster`；传了 `voiceprints` 时，簇中心与某人声纹的余弦 ≥ 0.62 就标成 `known`。
- `language`：请求里给了就用它，否则取各段猜测结果中说话时间最长的语言。
- 一个文件里各路声音已经混在一起，分不出流来源，所以 segment 不带 `origin`。
- 请求可能耗时较长；它和实时字幕共用同一个 GPU 队列，按段交替执行，录制中也可以调用。

## mm-embedd

| 方法 | 状态 | 说明 |
| --- | --- | --- |
| `model/load` `{directory}` | ✅ | 加载本地 EmbeddingGemma 2 检查点 |
| `model/status` / `model/unload` | ✅ | 卸载后显存还给实时字幕 |
| `embed/batch` `{items:[{text,title,images,video,audio}], task, dimensions}` | ✅ | `task`：`document`（默认）、`search_query`、`clustering`……；`dimensions`：768/512/256/128 |
