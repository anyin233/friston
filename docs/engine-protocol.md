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
| `permissions/status` | ✅ | `{screenCapture,microphone,calendar}`，值为 `granted` / `denied` / `notDetermined` / `restricted`；系统把引擎的权限算在启动它的 App 头上 |
| `permissions/request` `{kind}` | ✅ | 请求一项权限（`screenCapture` / `microphone` / `calendar`），返回请求后的状态 |
| `capture/start` | ✅ | 见下方“录制”；返回 `{sessionId,directory,files,microphoneChannels,startedAt}` |
| `capture/stop` | ✅ | 返回 `{sessionId,durationMs,files}`，`files` 只列出真正写入数据的文件 |
| `voiceprint/enroll` / `voiceprint/fromClip` | ✅ | 见下方“声纹”；返回声纹向量及质量指标 |
| `transcribe/file` | ✅ | 见下方“整段转写”；会后精转写（Qwen3-ASR 1.7B + Pyannote 分离） |
| `caption/simulateFile` | ✅ | 见下方“用文件模拟实时字幕”；不需要麦克风或屏幕录制权限 |
| `models/preload` | ✅ | 预先加载实时字幕模型，见“录制”一节 |
| `caption/partial` · `caption/final` · `speaker/relabel` | ✅ 通知 | 见下方“实时字幕” |
| `audio/levels` · `capture/state` | ✅ 通知 | 电平：`{sessionId,source,rmsDb[],peakDb[]}`，每路最多 100 ms 一次；状态：`{sessionId,state,error?}` |
| `spatial/cue` | ✅ 通知 | 方位，见下方“实时字幕” |

### 录制（`capture/start`）

参数（键名与 Swift 一致，Rust 端是 `mm_core::CapturePreset`）：

| 字段 | 说明 |
| --- | --- |
| `title` | 会议标题，用于文件夹名 `yyyy-MM-dd HHmm 标题`（`/`、`:` 替换为 `-`，缺省为“会议”） |
| `windowID` / `displayID` | 二选一；录指定窗口或整个显示器，HEVC 写入 `screen.mov` |
| `microphoneUID` | 来自 `audio/probeInputs`；保留全部硬件声道，Float32 写入 `mic.caf`。传 `"default"` 表示录系统当前的默认输入设备（在 `capture/start` 时确定，录制中途切换默认设备不影响本场）；没有默认输入时返回 `-32602`。不传则不录麦克风 |
| `systemAudio` | 录系统声音（不含本进程），48 kHz 立体声写入 `system.caf` |
| `captionLanguage` | 字幕语言提示（BCP-47，如 `zh`、`en-US`）；缺省、`auto` 或模型不支持的语言都交给模型自动识别 |
| `fps` | 1–60，默认 15 |
| `outputDirectory` | 覆盖默认根目录 `~/Movies/Friston` |
| `voiceprints` | 可选，预录声纹 `[{personId, model, vector:[Float]}]`；`model` 与 `initialize` 返回的 `capabilities.speakerEmbeddingModel` 不同的向量会被忽略 |
| `spatialFlip` | 可选，默认 `false`。左右翻转：交换方位分析用的两个声道，所有方位线索镜像 |
| `spatialSpacingCm` | 可选，两支麦克风的间距（厘米）。知道间距时引擎会给出 `azimuthDeg`；硬件预设（如 ReSpeaker）自带的间距优先 |

`initialize` 的 `capabilities.models` 列出实际使用的模型：`liveASR`、`offlineASR`、`vad`、`speakerEmbedding`、`segmentation`、`liveDiarization`（实时 Sortformer）。

`models/preload` `{language?}`：立即加载实时字幕要用的模型（与下文「实时字幕」加载的相同），不必等到第一次 `capture/start`。App 在引擎就绪后自动调用一次。返回 `{models:{角色: 模型 id}, unavailable:[角色]}`：`liveASR`、`vad` 必须成功，否则返回错误；`speakerEmbedding`、`liveDiarization`、`prefixDecoder`、`draft`（只在 `language` 为 `zh` 时加载）失败时列入 `unavailable`，字幕照常运行。模型已加载时立即返回。

同一时间只允许一场录制：录制中再次 `capture/start` 返回 `-32000`。状态顺序为 `starting → recording → stopping → stopped`，任一步出错为 `failed`，之后可以重新开始。`shutdown` 会先停止录制并收尾文件。

### 实时字幕

录制开始后，引擎在后台加载 Qwen3-ASR 1.7B 4bit、Silero VAD、WeSpeaker 和 Sortformer 流式分离模型（Sortformer 是 Core ML）。首次使用时由 speech-swift 下载到 `~/Library/Caches/qwen3-speech/`；Hugging Face 缓存里已有 `mlx-community/Qwen3-ASR-1.7B-4bit` 时直接使用。ASR 或 VAD 加载失败时只在 stderr 记日志，录制照常进行，只是没有字幕；WeSpeaker 失败时说话人为 `unknown`；Sortformer 失败时退回只用声纹向量（和方位）判断说话人，字幕照常。麦克风和系统声音各自切分、各自识别、各自运行一个 Sortformer 会话，流来源是说话人的硬分区。

- 断句：Silero VAD，停顿 ≥ 0.5 s 结束一句。一句超过 8 s 后，在其中最长的停顿（≥ 0.2 s，按能量判断，前后都有语音）处切开；到 12 s 仍没有停顿时，在最后 2 s 里最安静的 100 ms 处切开，不再切在词中间。
- VAD 确认开口后约 0.25 s 发第一条 `caption/partial`。之后有两种触发：一是按本机解码速度自适应刷新，间隔 = 最近 partial 解码耗时的滑动平均 × 2.5，限制在 0.2–1.5 s；二是说话中出现 ≥ 0.15 s 的词间停顿（按能量判断）时立即发一次，每个停顿只发一次，且距上一条 partial 至少有 0.1 s 新语音。处理落后于实时（一次取到 > 1.5 s 的音频）时跳过 partial，只保证 final。
- partial 用前缀回退解码：把同一句上一条 partial 的 token 去掉末尾 5 个，作为已知前缀和音频一起预填，只往后解码新的部分。所以前面的字不再变，每次只改末尾几个字，解码量也不随句子变长。`stableChars` 是这条 partial 里下一次会原样保留的前缀长度。final 仍然整句从头识别，结果可能改动前面的字。分词器文件缺失时退回旧做法（每次整句重识别，`stableChars` 为 0）。
- partial 的说话人只在音频满 1.5 s 时估计一次，之后的 partial 沿用，不再每次计算声纹。
- 草稿层（只在字幕语言为中文时加载）：一路流的 Qwen3-ASR partial 刷新间隔达到 0.6 s 以上时（机器慢或 GPU 被占用），这一路后续的句子改由 CPU 上的流式 Zipformer（sherpa-onnx）出 partial：每 320 ms 一块，文字只往后长，`stableChars` 为除最后一个字以外的全部。定稿仍由 Qwen3-ASR 整句识别。切换只发生一次，并写一行日志；一句话不会混用两种来源。环境变量 `MM_DRAFT=1` 强制使用草稿层，`MM_DRAFT=0` 不加载；`MM_DRAFT_DIR` 可指向本地的其他流式 transducer 模型目录（同样的四个文件），用于对比。
- 字幕停止时，引擎日志会写一行每路的解码统计（partial/final 解码耗时中位数与 p90、跳过的 partial 数、最终刷新间隔），用来判断这台机器是否跟得上实时。
- 说话人轮次：Sortformer 每 480 ms 处理一块音频（另需 560 ms 前瞻），给出最多 4 个说话人槽位的逐帧活动（80 ms 一帧）。槽位编号在本路流内稳定。final 会先等 Sortformer 确认到这句的结尾（通常晚 1–1.5 s，最多等 2.5 s）再发出，所以 final 比没有 Sortformer 时晚约 1 s；partial 不等。
- 换人切分：一句话里 Sortformer 听到换人（前后各有 ≥ 1 s 的语音）时，在换人处切开，交给融合分别判断说话人。两边判为同一人就合回一句（Sortformer 偶尔会把同一个人分到两个槽位）；判为不同的人就发多条 final：第一段沿用原 `utteranceId`（替换屏幕上的 partial），后面各段用新的 `utteranceId`。
- 说话人：每句 final（或切开后的每一段）计算 WeSpeaker 声纹，交给 `SpeakerFusion` 与预录声纹和本场的匿名簇比对；这句话里占主导的 Sortformer 槽位作为额外证据（见 [speaker-identification.md](speaker-identification.md)）；麦克风有 ≥ 2 个真实声道时再加上方位线索。短于 1.5 s 的句子只和已有的人比对、不新建说话人，比对不上或不足 0.4 s 时标为 `unknown`。
- 时间都是相对录制开始的毫秒数；`utteranceId` 在一场录制内跨两路唯一。

| 通知 | 参数 |
| --- | --- |
| `caption/partial` | `{sessionId, utteranceId, origin, startMs, text, speaker, stableChars}`，同一 `utteranceId` 的新 partial 替换旧的。`stableChars` 是 `text` 开头不会再变的 Unicode 标量（Rust `char`）个数，其余部分界面用浅色显示；旧引擎不发这个字段，按 0 处理 |
| `caption/final` | `{sessionId, utteranceId, origin, startMs, endMs, text, language?, speaker, spatial?}`，收到后撤掉该句的 partial |
| `spatial/cue` | `{sessionId, utteranceId, origin, startMs, endMs, position:[Float], confidence, azimuthDeg?, speaker}`，紧跟在同一句的 `caption/final` 之后 |
| `speaker/relabel` | `{sessionId, from, to}`：本场所有标为 `from` 的字幕改标为 `to`（匿名簇被认出是预录的人，或两个簇合并） |

- `origin`：`"microphone"` 或 `"system"`。
- `speaker`（与 `mm_core::SpeakerLabel` 相同，字段名用 snake_case）：`{"kind":"cluster","index":N}`、`{"kind":"known","person_id":"…","score":0.8}` 或 `{"kind":"unknown"}`。
- `language`：设置了 `captionLanguage` 时就是它的主语言码，否则按文字的书写系统猜（`zh`/`ja`/`ko`/`en`），猜不出时省略。
- `spatial`（与 `mm_core::SpatialCue` 相同）：`{azimuth_deg?, tdoa_samples, confidence}`，只在麦克风有方位线索时出现；`tdoa_samples` 是这句话里两声道时间差的中位数（麦克风采样率下的样本数，未做翻转）；`azimuth_deg` 只在 `capture/start` 给了 `spatialSpacingCm`（或硬件预设自带间距）时出现，含义同下面的 `azimuthDeg`。
- `spatial/cue`（Rust 端是 `mm_core::SpatialCueEvent`）：只对有方位线索、文字非空的 final 发出，麦克风少于 2 个真实声道时永远不会出现。
  - `position`：本场会议内的相对位置，三个坐标各约 −1…1，负为左（方位声道对的第一个声道一侧），正为右，已按 `spatialFlip` 翻转。`position[0]` 是左右坐标，按引擎判断的麦克风类型合并时间差和电平差（间隔式双麦用时间差，XY 重合式用电平差）；`position[1]`、`position[2]` 分别是时间差、电平差两个原始相对坐标。不同会议之间不可比较。
  - `confidence`：0…1，GCC-PHAT 平均置信度乘以可用帧占比。
  - `azimuthDeg`：相对正前方的角度，正为右，−90…90；只在知道间距时出现，并假定两路时钟同步（无线麦克风的链路延迟会让角度整体偏移）。
  - `speaker`：与这句 `caption/final` 相同；之后的 `speaker/relabel` 同样适用。
  - 两声道时间差突然整体跳变（例如无线发射器重连）时，引擎会重新学习相对位置的范围，约 4 秒语音后恢复；stderr 会记一行 `stereo delay jump`。
- 某句已经发过 partial、但最终识别为空时，仍会发一条 `text` 为空的 final，好让界面撤掉 partial。

### 用文件模拟实时字幕（`caption/simulateFile`）

参数 `{path, origin?, speed?, language?, voiceprints?, spatialFlip?, spatialSpacingCm?}`，返回 `{sessionId, origin, durationMs, finals:[…], relabels, spatialCues:[…]}`。

- 把音频文件按 100 ms 一块送进**和录制完全相同**的实时字幕流程（VAD 断句、Sortformer、WeSpeaker、融合），用来在没有麦克风或屏幕录制权限时测试字幕和说话人识别，包括系统声音这一路。
- `origin`：`"microphone"`（默认）或 `"system"`，决定字幕的流来源。
- 双声道文件按 `microphone` 送入时，保留两个声道和文件自己的采样率，像一个立体声麦克风一样跑方位分析并发出 `spatial/cue`；`spatialFlip`、`spatialSpacingCm` 与 `capture/start` 相同。其他情况（单声道、多于 2 声道、`system`）先混成单声道，没有方位线索。
- `speed`：1 为实时（默认），4 为四倍速，0 为尽快送完（此时会跳过 partial，只保证 final）。
- `language`、`voiceprints` 与 `capture/start` 的 `captionLanguage`、`voiceprints` 相同。
- 期间照常发出 `caption/partial`、`caption/final`、`speaker/relabel` 通知，`sessionId` 形如 `sim-xxxxxxxx`；时间是相对文件开头的毫秒数。不占用录制状态，录制中也可以调用。
- 结果里的 `finals` 是所有 final 通知，`spatialCues` 是所有 `spatial/cue` 通知，都已按之后的 `speaker/relabel` 改好标签；`relabels` 是发出的 relabel 条数。
- 文件不存在或 `speed` 为负返回 `-32602`；无法解码或字幕模型加载失败返回 `-32000`。

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

参数 `{path, language?, voiceprints?, model?, context?}`，返回 `{language?, segments:[{startMs, endMs, text, speaker}], model?}`。

- 先用 Pyannote 分离说话人（不加 Silero 预过滤：远场会议里 Silero 会漏掉约三分之一的语音，见 [asr-model-survey.md](asr-model-survey.md) 7.6 节），同一人间隔 ≤ 0.8 s 的片段合并，超过 25 s 的切开，再逐段用 Qwen3-ASR 1.7B 识别。分离模型不可用时退回只用 VAD 分段，`speaker` 为 `unknown`。
- 1.7B 优先使用 Hugging Face 缓存里的 `mlx-community/Qwen3-ASR-1.7B-8bit`，没有时由 speech-swift 下载 `aufklarer/Qwen3-ASR-1.7B-MLX-8bit`。模型只在请求期间驻留内存。
- `model`：可选，换用别的 Qwen3-ASR MLX 模型（会后精转可以比实时字幕用更大的模型）。可以是 Hugging Face 仓库名（`owner/name`，缓存里有就离线加载，否则由 speech-swift 下载）或本地文件夹的绝对路径（Hugging Face 快照文件夹按仓库名识别）。名字里必须有 `Qwen3-ASR` 和 `0.6B`/`1.7B`，因为 speech-swift 按名字选择网络结构和量化位数；不满足时在分离之前就返回 `-32602`。结果里的 `model` 是实际使用的模型。
- `context`：可选，给解码器的提示文字（会议标题、参会人名、笔记里的术语），放进 Qwen3-ASR 的 system 提示，能明显改善人名和专有名词。超过 600 字截断。
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
