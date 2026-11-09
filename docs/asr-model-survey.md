# 支持 MLX 的开源 ASR 模型调研（2026-11）

目标：给实时字幕找一个**中文准确率最好**、**延迟最低**、能在 Apple Silicon 上用 MLX 跑的开源模型。

结论先行：**中文会议场景继续用 Qwen3-ASR**。实时字幕可以考虑从 0.6B 升到 1.7B。只有在以英文为主、需要亚秒级字幕时，才值得加 Nemotron 3.5 流式。

## 1. 候选与 MLX 可用性

| 模型 | 参数 | 中文 | 流式方式 | MLX 实现 | 许可 |
| --- | --- | --- | --- | --- | --- |
| **Qwen3-ASR 0.6B / 1.7B** | 0.6B / 1.7B | 普通话 + 22 种方言 | 动态注意力窗口，2 s 分块（官方只在 vLLM 上开放流式） | speech-swift `Qwen3ASR`（本项目在用）、mlx-audio、funasr-mlx | Apache-2.0 |
| **Nemotron 3.5 ASR Streaming** | 0.6B | 有，但只是 “broad-coverage” 档 | 原生 cache-aware RNNT，分块 80–1120 ms | speech-swift `NemotronStreamingASR`（CoreML + MLX）、社区 mlx-rs 版 | NVIDIA OpenMDW-1.1 |
| **Voxtral Mini 4B Realtime 2602** | 4B | 有（13 种语言之一） | 原生因果编码器，延迟 80 ms–2.4 s 可调 | mlx-audio `voxtral_realtime`（需 4/8-bit） | Apache-2.0 |
| **Fun-ASR-Nano-2512** | 0.8B | 中/英/日 + 7 种方言，支持热词 | 无原生流式，只能伪流式 | mlx-audio、funasr-mlx、OminiX-MLX | Apache-2.0 |
| SenseVoice Small / Paraformer | 0.2B 级 | 强 | Paraformer 有流式版 | funasr-mlx（流式 Paraformer 在 MLX 上数值漂移，CER 0.156） | 模型许可 |
| Kyutai STT | 1B / 2.6B | **不支持** | 原生流式 | speech-swift `KyutaiSTT` | CC-BY-4.0 |
| Parakeet TDT 0.6B v3 / EOU 120M | 0.6B / 0.12B | **不支持**（25 种欧洲语言） | EOU 版流式 | speech-swift 走 CoreML，parakeet-mlx | CC-BY-4.0 |
| Cohere Transcribe 2B | 2B | 未公布普通话指标 | 无 | speech-swift、mlx-audio | Apache-2.0 |
| Whisper large-v3(-turbo) | 0.8–1.55B | 一般 | 无 | mlx-whisper 等 | MIT |

## 2. 中文准确率（越低越好）

| 模型 | FLEURS-zh | WenetSpeech net / meeting | AISHELL-2 | 说明 |
| --- | --- | --- | --- | --- |
| Qwen3-ASR 1.7B 离线 | 2.41 | 4.97 / 5.88 | 2.71 | 官方技术报告 |
| Qwen3-ASR 1.7B **流式** | **2.84** | – | – | 2 s 分块 |
| Qwen3-ASR 0.6B 离线 | 2.88 | 5.97 / 6.88 | 3.15 | |
| Qwen3-ASR 0.6B **流式** | 3.40 | – | – | |
| Fun-ASR-MLT-Nano | 3.51 | 6.35 / – | – | 引自 Qwen3-ASR 报告 |
| Whisper large-v3 | 4.09 | 9.86 / 19.11 | 5.06 | |
| Voxtral Realtime @480 ms | 10.45 | – | – | @960 ms 为 8.99，@2.4 s 为 8.48 |
| Nemotron 3.5 Streaming | ≈19–21 | – | – | 第三方转引 NVIDIA 数据 |

注意：各家指标来自不同来源，测试条件不完全一致；但 Qwen3-ASR 和其他流式模型在中文上相差约 3–7 倍，这个量级的差距足以作结论。WenetSpeech-meeting 最接近本项目的会议场景，Qwen3-ASR 在这一项上优势也最大。

## 3. 实时性

| 模型 | Mac 上实测或报告的速度 | 首字 / 跟随延迟 |
| --- | --- | --- |
| Nemotron 3.5 Streaming | M2 Max 单路 RTF 0.089（320 ms 分块，MLX fp16 编码器） | 分块即延迟：80–1120 ms，不重复计算 |
| Voxtral Realtime 8-bit | M5 Pro 能跟上实时（fp16 跟不上，0.47 倍实时） | 首字约 1.8 s，说完后约 0.3 s 出终稿 |
| Qwen3-ASR 0.6B 4bit（本项目） | 能跟上实时；会后 1.7B 8bit 约 0.2–0.3 倍实时 | 取决于 VAD 切句与部分结果刷新间隔；伪流式每次重算已缓冲音频 |
| Fun-ASR-Nano w4 | RTF 约 0.10 | 伪流式，同上 |

延迟最低的是 **Nemotron 3.5**（真正的增量计算，延迟 ≤ 1 s 可调），但它的中文 WER 约 20%，不适合中文会议。Voxtral Realtime 在中文上也明显落后于 Qwen3-ASR，而且 4B 参数要占 2.5–4.6 GB 显存。

## 4. 建议

1. **主力不换：Qwen3-ASR。** 在 MLX 可用的开源模型里，它是中文（含方言、会议场景）准确率最高的，流式模式的损失也小（1.7B：2.41 → 2.84）。speech-swift 已经集成，不需要新增依赖，也不碰两个引擎的版本锁定。
2. **可评估：实时字幕从 0.6B 换成 1.7B。** 流式 FLEURS-zh 3.40 → 2.84。代价是算力与内存；1.7B 在会后转写里是 0.2–0.3 倍实时，用于伪流式时需要在开发机上实测部分结果的刷新耗时，再决定是否只在 M 系列 Pro/Max 上默认开启。
3. **可选：英文或多语种会议加 Nemotron 3.5 Streaming。** speech-swift 0.0.29 已经带 `NemotronStreamingASR`，接入成本低；按语言识别结果路由，中文仍走 Qwen3-ASR。
4. **不建议：** Voxtral Realtime（中文差、体积大、只有 Python 的 mlx-audio 实现，不适合 Swift 引擎）；Kyutai / Parakeet（不支持中文）；MLX 版流式 Paraformer（数值漂移）。
5. **Fun-ASR-Nano 值得留意**：它的热词能力对人名、术语有用，但目前只有 Python MLX 实现，已公布的中文指标也不如 Qwen3-ASR 0.6B（FLEURS-zh 3.51 对 2.88），暂不引入。

第 2、3 节的数字来自公开报告；第 5 节是在本项目实时字幕管线上的实测。

## 5. 实测：实时字幕 0.6B 对 1.7B（2026-11-06，M5 Pro 48 GB）

**方法。** 从 AISHELL-1 测试集随机取 70 句（约 7 个说话人），句间插入 0.6–1.2 秒静音，拼成 428 秒的 16 kHz 文件。用 Release 版 `mm-meetingd` 的 `caption/simulateFile` 按实时速度回放，语言设为中文，经过完整的实时管线（Silero VAD、部分结果、Sortformer、WeSpeaker）。先用 12 秒片段预热，让模型加载完；三个模型依次单独运行。字错误率去掉标点和空格后，把全部定稿文本拼起来与参考文本比较。延迟以参考文本中每句的真实起止时间为准。

| 模型 | 字错误率 | 定稿延迟（句子结束 → 定稿）中位 / p90 / 最大 | 第一条部分结果（开口 → 显示）中位 / p90 | 进程内存峰值 |
| --- | --- | --- | --- | --- |
| Qwen3-ASR 0.6B 4bit（现用） | 4.78% | 0.77 / 0.88 / 1.83 秒 | 1.15 / 4.45 秒 | 0.82 GB |
| Qwen3-ASR 1.7B 4bit | **3.63%** | 0.81 / 0.95 / 1.28 秒 | 1.18 / 4.71 秒 | 1.66 GB |
| Qwen3-ASR 1.7B 8bit | 3.63% | 0.84 / 0.96 / 1.36 秒 | 1.18 / 4.47 秒 | 2.49 GB |

**结论。**
- 1.7B 的字错误率相对下降约 24%，改正的大多是同音字、人名和专有名词（纵火/送火、情变/勤辩、站立/战力、昔年/西年）。
- 在 M5 Pro 上，延迟几乎不受模型大小影响：定稿延迟主要来自 VAD 的 0.5 秒静音判定和等待 Sortformer，1.7B 只多出约 40–70 毫秒。每句的部分结果次数也一样（约 3.7 次），说明 1.7B 没有跟不上实时。
- 1.7B 的 4bit 和 8bit 识别结果完全一致，4bit 少占 0.8 GB，所以如果切换，应该选 **1.7B 4bit**（`mlx-community/Qwen3-ASR-1.7B-4bit`）。
- 两个模型都把两句截断了（“…最大”“…打出我感觉”）。这与模型大小无关，像是切句或拆分的问题，值得单独排查。

**局限。** AISHELL-1 是朗读的新闻，比真实会议干净，也没有重叠说话；字错误率的绝对值会比会议录音低。只测了 M5 Pro，在 M1/M2 基础款上 1.7B 是否仍能跟上实时没有验证。

**已采用。** 2026-11-06 起实时字幕改用 `mlx-community/Qwen3-ASR-1.7B-4bit`（`SpeechModelIDs.liveASR`）。

**附带修正。** 第一轮测试发现 `caption/simulateFile` 每 100 ms 固定睡眠 100 ms，回放比实时慢约 4%，7 分钟后延迟累积到约 17 秒，而且三个模型完全相同。现在改为按时钟定步（`SimulateCaptions.swift`），修正后延迟在整段回放中保持不变。`tech-stack.md` 里此前约 3.6 秒的定稿延迟也受这个问题影响，已经更新。

## 6. 实时性优化与会后精转（2026-11-07）

**实时字幕。** 第 5 节的“第一条部分结果 p90 4.5 秒”和固定 1 秒刷新，是实时字幕不够“实时”的主要原因；模型本身很快：1.7B 4bit 在 M5 Pro 上每次 partial 解码中位约 0.1 秒。所以改的是调度，不是模型：

- partial 刷新按实测解码耗时自适应（耗时 × 2.5，0.3–1.5 秒），快机器约 0.3 秒刷新一次，慢机器自动放慢，不会拖垮 final；
- 第一条 partial 从开口后 0.5 秒提前到 0.4 秒；
- 长句超过 8 秒时在最长的停顿处切开，12 秒仍无停顿时在最安静处切开（原来固定在 12 秒切，常切在词中间，定稿也被拖到 8 秒以后）。

用 `scripts/eval-captions.py`（macOS 自带的婷婷语音，按语速模拟 4 人，68 秒 12 句）实测，Release 引擎：

| 指标 | 优化前 | 优化后 |
| --- | --- | --- |
| partial 刷新间隔（中位） | 1.03 s | 0.33 s |
| 句末到屏幕出现最后几个字（中位 / p90） | 0.51 / 1.19 s | −0.02 / 0.07 s |
| 第一条字幕（开口 → 显示，中位 / p90） | 0.91 / 1.66 s | 0.75–0.82 / 1.09 s |
| 定稿延迟（中位 / 最大） | 1.15 / 8.3 s | 1.15 / 3.4 s |
| 字错误率 | 0 | 0 |

定稿延迟主要来自 VAD 的 0.5 秒静音判定和等待 Sortformer 确认，没有动。合成语音很干净，字错误率的绝对值没有参考意义，这里只用来确认切句没有把字切坏。

**会后精转。** speech-swift 0.0.29 的 Qwen3-ASR 解码器只支持 4/5/8 bit 量化权重，最大就是 1.7B 8bit，所以“更强”从两处来：
- 精转模型和实时模型分开：设置里的“精转模型”可以填任何 Qwen3-ASR MLX 仓库或本地文件夹（`transcribe/file.model`），实时字幕不受影响；填的模型加载失败时自动退回默认模型并提示。
- 上下文提示：会议标题、参会人、已登记声纹的人名和用户笔记作为 Qwen3-ASR 的 context 传入（`transcribe/file.context`，≤ 600 字）。实时字幕不带提示，所以人名和术语的同音字主要靠精转纠正。

## 7. 会后精转：有没有比 Qwen3-ASR 更强的模型（2026-11-07）

会后精转没有实时约束，可以用更大的模型、更慢的解码，所以这一节不看流式能力，只看三件事：**中文会议准确率**、**能否在 Mac 本地跑**、**接入成本**（是否已经在 speech-swift 里，会不会碰 `mlx-swift-lm` exact 3.31.4 的锁定）。

结论先行：
- **公开榜单上更强的开源模型是 FireRedASR2，但实测没有赢。** 按公开数据，1.18B 的 AED 版在 WenetSpeech-meeting 上 4.53，8B 的 LLM 版 4.32，Qwen3-ASR 1.7B 是 5.88。可是在 AISHELL-4 真实会议上实测（7.5 节），FireRedASR2-AED 的整体字错误率反而更高（17.71% 对 16.52%）：单人说话时它更准，多人重叠说话时更差。**精转继续用 Qwen3-ASR。** 分段问题已经修正（7.6 节），真实会议上的字错误率从 32% 降到 18%。
- **其他新模型在会议集上没有赢**：小米 MiMo-V2.5-ASR（8B）在 WenetSpeech-meeting 上 5.92，和 Qwen3-ASR 持平；微软 VibeVoice-ASR（9B）中文会议明显更差；闭源的 Fun-ASR v1.5（30B MoE，仅 API）、Seed-ASR 2.0、Gemini 3.1 Pro 在会议集上也不比 Qwen3-ASR 好。
- **MOSS-Transcribe-Diarize 0.9B 值得单独评估**，但它的价值在“谁说了什么”而不是字准确率：一次处理最长 90 分钟，同时输出说话人和时间戳。项目已锁定的 speech-swift 0.0.29 里**已经带了** `MossTranscribe`（MLX INT5/INT8 + CoreML），接入成本最低。

### 7.1 候选

| 模型 | 参数 | 许可 | Mac 本地运行 | 上下文 / 热词 | 长音频 | 说话人 |
| --- | --- | --- | --- | --- | --- | --- |
| Qwen3-ASR 1.7B（现用） | 1.7B | Apache-2.0 | speech-swift `Qwen3ASR`（在用） | 有（context） | 分段 | 无（靠 Sortformer + WeSpeaker） |
| **FireRedASR2-AED** | 1.18B | Apache-2.0 | mlx-audio `fireredasr2`（Python，`mlx-community/FireRedASR2-AED-mlx`）；sherpa-onnx int8（有 Swift/C API，macOS arm64） | **无** | 需按 VAD 分段 | 无；但输出逐字时间戳和置信度 |
| FireRedASR2-LLM | 8B+（编码器 + Qwen2-7B-Instruct） | Apache-2.0 | 只有 PyTorch / vLLM，**没找到 MLX 移植** | 无公开接口 | 需分段（v1 写明 LLM 输入 ≤ 30 秒） | 无 |
| MiMo-V2.5-ASR | 8B + 1.2B 音频 tokenizer | 见上游模型卡 | 社区 MLX 移植（`mlx-community/MiMo-V2.5-ASR-MLX-8bit`、`mimo-mlx`，均为 Python） | 无 | 分段 | 无 |
| VibeVoice-ASR | 9B | MIT | mlx-audio（`mlx-community/VibeVoice-ASR-4bit`，5.7 GB；Python） | 有（hotwords） | 单次 60 分钟 | 有 |
| **MOSS-Transcribe-Diarize** | 0.9B | Apache-2.0 | **speech-swift 0.0.29 `MossTranscribe`**（MLX INT5 / INT8、CoreML） | 未见 | 单次最长 90 分钟（128K 上下文） | 有，含时间戳 |
| Fun-ASR v1.5 | 30B MoE | 闭源，仅阿里云 API | 否 | – | – | – |

### 7.2 中文准确率（CER %，越低越好）

| 模型 | WenetSpeech-meeting | WenetSpeech-net | AISHELL-2 | AISHELL-1 | 来源 |
| --- | --- | --- | --- | --- | --- |
| Qwen3-ASR 1.7B | 5.88 | 4.97 | 2.71 | 1.48 | FireRedASR2S 报告 |
| Qwen3-ASR 1.7B | 5.90 | 4.94 | 2.67 | – | MiMo 博客 |
| **FireRedASR2-LLM** | **4.32** | **4.44** | **2.15** | 0.64 | FireRedASR2S 报告 |
| **FireRedASR2-AED** | 4.53 | 4.57 | 2.51 | **0.57** | FireRedASR2S 报告 |
| Doubao-ASR（闭源） | 4.74 | 5.73 | 2.77 | 1.52 | FireRedASR2S 报告 |
| Fun-ASR（闭源） | 5.78 | 6.85 | 2.38 | 1.64 | FireRedASR2S 报告 |
| MiMo-V2.5-ASR | 5.92 | 5.26 | 2.52 | – | MiMo 博客 |
| Fun-ASR v1.5（闭源） | 5.95 | 5.28 | 2.57 | – | MiMo 博客 |
| Seed-ASR 2.0（闭源） | 7.22 | 4.89 | 2.63 | – | MiMo 博客 |
| Gemini 3.1 Pro（闭源） | 12.09 | 9.69 | 4.52 | – | MiMo 博客 |

真实会议录音（远场、重叠说话）上的说话人归属转写，来自 MOSS 模型卡（CER / cpCER，cpCER 同时计入说话人错误）：

| 模型 | AISHELL-4 CER / cpCER | AliMeeting CER / cpCER |
| --- | --- | --- |
| MOSS-Transcribe-Diarize 0.9B | **14.84 / 15.83** | **24.86 / 22.17** |
| VibeVoice-ASR | 21.40 / 24.99 | 27.40 / 29.33 |
| Doubao | 18.18 / 27.86 | 25.25 / 37.57 |
| Gemini 3 Pro | 22.75 / 27.43 | 26.75 / 32.84 |

注意：
- 两张表的 Qwen3-ASR 数字在两家报告里基本一致（5.88 / 5.90），说明第一张表可以横向比较；但 FireRedASR2 的数字是它自己报告的，没有第三方复现。
- MOSS 的表里**没有 Qwen3-ASR**，AISHELL-4 是 40 分钟的远场会议，绝对值和 WenetSpeech 不能比。MOSS 0.9B 的纯识别能力是否强于 Qwen3-ASR 1.7B，只能自己测。
- 一篇 2026-08 的蒸馏论文（arXiv 2608.03610）在自己的评测条件下测出 Qwen3-ASR 1.7B 的 WenetSpeech-meeting 为 4.13，说明测评条件（分段、文本归一化）对绝对值影响很大，**必须在同一套脚本下自己比**。

### 7.3 逐个判断

**FireRedASR2-AED：最值得引入。** 会议集上比 Qwen3-ASR 1.7B 低约 23%，参数还更少（1.18B），Apache-2.0。另外两点对精转有用：逐字时间戳（可以省掉 ForcedAligner 这一步）和句级置信度（可以标出存疑的句子）。缺点：
- 没有 context / 热词接口，第 6 节的人名提示用不上。精转对人名的纠正可能反而变差，要实测。
- speech-swift 里没有。接入有两条路：(a) 用 sherpa-onnx 的 C API（已有 FireRedASR2 int8 模型，跑 CPU，不碰 MLX 版本锁定）；(b) 参照 mlx-audio 的 Python 实现写 MLX Swift 版（Conformer 编码器 + Transformer 解码器 + beam search，结构不复杂，但要自己维护）。
- 编码器-解码器模型有输入长度上限，需要按 VAD 分段，分段处的字可能出错。

**FireRedASR2-LLM：不值得。** 比 AED 只再好 0.2 个点（4.32 对 4.53），却要 8B+ 参数，也没有 MLX 移植。

**MiMo-V2.5-ASR：不值得。** 优势在方言（吴语 19.55 对 24.29）、FLEURS（2.41 对 3.21）和中英混说，会议集上和 Qwen3-ASR 持平。8B + 1.2B tokenizer 的体积是 Qwen3-ASR 的 5 倍以上，只有 Python 版 MLX 移植。

**VibeVoice-ASR：不值得。** 单次 60 分钟、自带说话人的思路很好，但中文会议 CER 比 MOSS 高约 45%，英文也不如 Qwen3-ASR（Open ASR 平均 WER 7.77 对 5.76）；4bit 版处理 1 小时音频峰值内存约 30 GB（M5 Max 实测），还有长音频重复输出（“Yes. Yes. …”死循环）的公开问题。

**MOSS-Transcribe-Diarize：值得评估，但评估的是说话人管线。** 它在 AISHELL-4 上 CER 和 cpCER 只差 1 个点，说明说话人归属几乎不额外出错；而我们现在是 ASR + Sortformer + WeSpeaker + 全局聚类，误差会叠加。speech-swift 在 M5 Pro 上实测 INT5 约 35 倍实时、峰值内存约 1.2 GB（英文 FLEURS），已经在依赖里，不需要动版本锁定。风险：它的说话人标签是每段录音内的匿名标签，仍要和声纹库做映射；speech-swift 的测评只做了英文，中文要自己测。

**闭源 API（Fun-ASR v1.5、Doubao、Seed-ASR 2.0、Gemini）：不考虑。** 会议集上没有优势，还要把录音上传到云端。

### 7.4 建议

1. **先补一个“会议”精转评测集**，再决定换不换。第 5 节用的 AISHELL-1 是朗读新闻，太干净，FireRedASR2 在 AISHELL-1 上的领先（0.57 对 1.48）不代表会议场景。用 AISHELL-4 和 AliMeeting 的测试集（真实会议、远场、重叠说话，自带说话人标注），同时记 CER、cpCER、耗时和内存峰值。可以在 `scripts/eval-diarization.py` 的基础上扩展。
2. **第一个对比：Qwen3-ASR 1.7B 8bit（带 / 不带 context）对 FireRedASR2-AED。** 先用 mlx-audio 或 sherpa-onnx 在 Python 里离线跑，不改引擎。如果 FireRedASR2-AED 在真实会议上也能领先 15% 以上，而且人名错误没有明显增加，再决定走 sherpa-onnx 还是写 MLX Swift 版。
3. **第二个对比：MOSS-Transcribe-Diarize INT5 对现在的整条说话人管线**，看 cpCER。它已经在 speech-swift 0.0.29 里，可以直接在 `mm-meetingd` 里加一个实验性的 `transcribe/file` 后端。
4. **如果两个都有优势，可以组合**：MOSS 给说话人分段和时间，FireRedASR2-AED 或 Qwen3-ASR（带 context）在每段上重新识别文本。这一步要等前两步的数据。

（以上是实测前的建议，第 1、2 条已在 7.5 节完成，结论见 7.5。）

### 7.5 实测：AISHELL-4 上 Qwen3-ASR 对 FireRedASR2-AED（2026-11-07，M5 Pro 48 GB）

**数据。** 从 AISHELL-4 测试集（20 场）中选 4 场，大、中、小会议室都有：`L_R003S01C02`、`L_R004S01C01`、`M_R003S01C01`、`S_R003S01C01`。共 151 分钟，35,914 个参考字，每场 4–7 人，真实会议，多人重叠说话很多（49% 的参考字落在重叠区域）。只取 8 路阵列的第 0 路（16 kHz 单声道），接近笔记本单麦克风的情况。

**方法。** 脚本 `scripts/eval-refine.py`，分两组：
- **引擎管线**：Release 版 `mm-meetingd` 的 `transcribe/file`（Pyannote 分段 + Qwen3-ASR 1.7B 8bit，`language: zh`，不带 context）；FireRedASR2-AED 识别引擎给出的同一批分段，两者只差识别模型。
- **参考分段**：按标注的说话时间切音频（重叠或间隔 < 0.3 秒的标注合并成一段，单段 ≤ 25 秒），两个模型都用 mlx-audio 识别。Qwen 用和引擎相同的 `mlx-community/Qwen3-ASR-1.7B-8bit`，FireRed 用 `mlx-community/FireRedASR2-AED-mlx`（fp32，beam 3）。这一组去掉了分段误差，只比较识别模型。

字错误率：去掉标注符号（`<sil>` 等）、标点和空格后，把各段文本按时间拼接，再和拼接后的参考文本比较。假设文本里的阿拉伯数字转成汉字（“001”按位转成“零零一”），因为 AISHELL-4 的参考文本用汉字写数字。

| 设置 | Qwen3-ASR 1.7B 8bit | FireRedASR2-AED | 说明 |
| --- | --- | --- | --- |
| 引擎管线（同一批分段） | 32.19% | 31.31% | 两者的删除错误都在 8,300 字以上，主要来自分段 |
| 参考分段 | **16.52%** | 17.71% | FireRed 的替换少 28%，删除多 46% |
| 参考分段 · 单人说话区域 | 11.39% | **9.78%** | FireRed 相对低 14% |
| 参考分段 · 重叠说话区域 | **22.05%** | 26.24% | FireRed 只转写最响的人，其他人的话被删掉 |
| 识别耗时（参考分段，倍实时） | **0.045** | 0.139 | 两者都在 mlx-audio 上运行，FireRed 是 fp32 权重、beam 3 |

各场的参考分段字错误率（Qwen / FireRed）：L_R003 21.70 / 23.70，L_R004 17.39 / 17.55，M_R003 16.99 / 17.67，S_R003 10.57 / 12.48。四场都是 Qwen 领先。去掉语气词（啊、嗯、呃等）后，结论不变（单人区域 10.64 / 9.04，重叠区域 20.12 / 24.50）。

**结论。**
1. **不换成 FireRedASR2-AED。** 在真实会议的整体字错误率上，它没有超过 Qwen3-ASR，而且在 mlx-audio 上慢约 3 倍，也没有 context 接口。公开榜单上它在 WenetSpeech-meeting 领先约 23%，主要来自单人清晰语音；我们实测单人区域也只领先 14%，重叠说话反而落后 19%。
2. **精转的最大问题是分段，不是识别模型**（已在 7.6 节修正，32.19% → 18.13%）。 同一个 Qwen3-ASR，引擎分段的字错误率是 32.19%，参考分段只有 16.52%，几乎差一倍。引擎的分段只覆盖了 54%–90% 的参考语音（M_R003 那场有 27% 的参考字所在的句子整句没有进入任何分段），删除错误占总错误的 79%。下一步应该排查 Silero 预过滤和 Pyannote 分段在远场、多人场景下的漏检，而不是换模型。
3. **可以考虑的组合**（优先级低于第 2 条）：Pyannote 本来就能判断重叠区域，单人区域用 FireRedASR2、重叠区域用 Qwen3-ASR。按上表估算，参考分段下的整体字错误率约从 16.5% 降到 15.7%，收益不大，还要再维护一个模型。

**局限。**
- 只测了 20 场中的 4 场、单路远场麦克风；没有测 AliMeeting。
- AISHELL-4 没有会议标题和人名，测不了 context 提示的作用；Qwen 实际用时带 context，会比这里更好。
- 参考文本的写法会影响结果：Qwen 删除最多的是“儿”（446 次，儿化音参考写出来、Qwen 不写），FireRed 删除最多的是“嗯”“对”“是”。
- FireRed 的耗时只有 mlx-audio 的 fp32 版，量化版或 sherpa-onnx 版可能更快，但不影响准确率结论。

### 7.6 精转分段漏检：去掉 Silero 预过滤（2026-11-08）

**定位。** 在引擎里临时导出 `transcribe/file` 每一步的分段，对照 AISHELL-4 标注，计算每一步覆盖了多少参考语音：

| 分段 | 参考语音覆盖 | 参考字覆盖 | 说明 |
| --- | --- | --- | --- |
| Silero VAD（整段） | 67.7% | 69.8% | 远场、小声和重叠说话大量漏检；M_R003 只有 53.6% |
| Pyannote，不加预过滤 | **97.4%** | **97.7%** | 误标为语音的非语音约 2.5% |
| Pyannote + Silero 预过滤（原来） | 67.4% | 69.8% | speech-swift 只保留与 Silero 重叠 ≥ 50% 的分段，等于用 Silero 的漏检把 Pyannote 截掉 |
| 合并成识别窗口后（原来） | 70.9% | 73.3% | 送进 Qwen3-ASR 的就是这些 |

所以漏检来自 Silero 预过滤，而不是 Pyannote，也不是窗口合并。预过滤是最初接入时照 speech-swift 的示例打开的，没有记录理由。

**修正。** `transcribe/file` 的 Pyannote 管线不再加 Silero 预过滤（`TurnPlanner.sileroPrefilter = false`）。分离模型不可用时退回的纯 VAD 分段仍然用 Silero。

**复测**（同样 4 场、同样的脚本和 Release 引擎）：

| | 字错误率 | 替换 / 删除 / 插入 | 倍实时 |
| --- | --- | --- | --- |
| 原来（有预过滤） | 32.19% | 2,615 / 8,347 / 598 | 0.103 |
| **现在（无预过滤）** | **18.13%** | 2,901 / 2,532 / 1,080 | 0.105 |
| 参考分段（上限） | 16.52% | 2,620 / 2,530 / 782 | – |

各场：L_R003 39.58 → 24.01，L_R004 28.48 → 17.86，M_R003 48.15 → 19.04，S_R003 14.62 → 12.25。删除错误减少 70%，已经和参考分段持平。插入错误多了 482 个，来自多送进去的音频，以及 Pyannote 把约 2.5% 的非语音标成语音；相比删除错误的减少可以接受。识别到的说话人数和以前差不多（每场 3–7 人，参考 5–7 人），耗时不变。

**剩下的差距**（18.13% 对 16.52%）主要是插入错误和不同说话人窗口的重叠：每场有 8–44 秒音频被两个说话人的窗口重复识别。

**实时字幕也用 Silero 断句**，远场时可能同样漏检。不过实时场景多是近讲麦克风，这次没有测。

## 8. 边说边出字：Foresight 的做法与本项目的差距（2026-11-08）

第 6 节把 partial 刷新压到约 0.33 秒，但字幕仍然是“一段一段跳出来”，而不是随着说话一个字一个字长出来。为了找出差距，拆解了本机的 Google AI Edge Foresight（`/Applications/Foresight.app`，1.0.0，构建号 1.0.0.28.20261005）。以下结论来自二进制里的 protobuf 描述、日志字符串和本地模型文件，**没有反编译代码逻辑**，数值默认值也拿不到。

### 8.1 Foresight 的转写管线

**组成。** Flutter 界面加 Google 内部的 `odml/audio/transcription` C++ 库，推理用 LiteRT-LM（Metal 后端）。

| 模型文件（`~/Library/Application Support/com.google.AIEdgeForesight/models/`） | 大小 | 判断 |
| --- | --- | --- |
| `3/1.1/*.litertlm` | 178 MB | 实时 ASR，模型类型 `TinyGemmaAsr`：USM 风格音频编码器加小 Gemma 解码器。用 Tink 加密，缓存里有 `streaming_audio_encoder` 和 `audio_adapter` |
| `16/2.0/*.litertlm` | 6.4 GB | Gemma 4 系列大模型，带 MTP 草稿头（`mtp_drafter`），负责笔记、摘要和问答 |
| `1/3.0/*.litertlm` | 445 MB | 多模态嵌入或小模型（文本、图像、音频编码器），用于 RAG |
| `4/1.1/*.tflite`、`2/1.0/*.model` | 171 MB、4.5 MB | 可能是 NAM 后处理（见下文）或嵌入模型的 tflite 与 SentencePiece 词表，无法确认 |

LiteRT-LM 的 `AsrModelType` 里同时有 `TinyGemmaAsr` 和 **`Qwen3Asr`**，说明谷歌的运行时也支持 Qwen3-ASR，下面的增量方法可以直接套用在 Qwen3-ASR 上。

**管线（`LiteRtLmSpeechProcessorOptions` 的字段和日志里的函数名）：**

1. **VAD 分段。** 使用 WebRTC RNN VAD（rnnoise，10 ms 帧）。`VadSegmenterOptions` 有 `start_trigger_duration_ms`、`end_trigger_duration_ms`、`lookahead_duration_ms`、`min_onset_threshold`、`max_offset_threshold`（双阈值滞回），事件有 `VAD_SPEECH_ONSET`、`VAD_SPEECH_OFFSET` 和 `VAD_SPEECH_RESUMED`。
2. **流式音频编码。** 模型里有单独的 `streaming_audio_encoder`，执行器为 `AudioLiteRtCompiledModelExecutor::AudioStreamingEncoder`，状态保存在 `AudioStreamingContext`。编码器参数有 `block_size`、`past_horizon`、`future_horizon`、`pad_for_streaming` 和 `tokens_per_frame`，也就是带有限右看的块状因果注意力。每来一块频谱就编码一次，用 `AddAudioSpectrum` **追加**进 LLM 会话的 KV 缓存（`prefilled_spectrogram_columns`），已经处理过的音频不再重算。
3. **interim 结果（临时字幕）。** 日志里有 `ProcessInterimSegment` 和 `cloned_session->AddQueryChunk(TranscriptTokensToString(*transcript_tokens_prefill))`，参数有 `decoder_prefill_backoff` 和 `extra_decode_tokens`，据此推断流程如下：
   - **克隆**已经装好音频的会话，主会话继续接收新音频；
   - 把**上一次的转写结果去掉末尾 N 个 token**（backoff）当作已知前缀一次性预填；
   - 只往后解码若干个新 token（`extra_decode_tokens`），得到 interim 后丢弃克隆的会话。

   这样前面的字不会变，**只有末尾几个字会被改写**，Foresight 的“实时改字”就是这么来的。解码步数从“整句的 token 数”降到“回退数加新增数”。
4. **触发时机。** 除了按块刷新，还有 `interim_results_trigger_silence_ms`：在**短停顿**处（比断句静音短，代码校验它必须小于 `end_trigger_duration_ms`）立即出一次 interim，也就是在词边界出字，不会卡在半个词上。
5. **定稿。** VAD 判定说完后执行 `ProcessFinalSegment`。由 `final_prefill_disabled`、`final_prefill_same_as_interim` 和 `final_decoder_prefill_backoff` 决定：定稿是整句重新解码（可以改前面的字），还是沿用前缀只重解尾部。结果带 `is_final`，覆盖同一段的 interim。
6. **界面层。** Dart 端有 `activeInterims`、`_areInterimsEqual`（相同的 interim 不重绘）、`flushPendingInterims` 和 `LiveTranscriptNotifier`。
7. **NAM 后处理。** `NamPostProcessorOptions`（attention、text encoder、spm 三个模型）是谷歌的 Neural Associative Memory 上下文偏置，用来纠正人名和术语。

**本机的“Foresight Multilingual”补丁**（`~/Applications/Foresight Multilingual.app`）把 `OdmlSpeechProcessorTranscribe` 换成了经 Unix socket 调用 Python mlx-audio 的 Qwen3-ASR，**每段说完才整段识别一次**，所以它没有 interim，看不到边说边出字的效果。要体验原版效果，需要直接打开 `/Applications/Foresight.app`（英文模型）。

### 8.2 本项目实时字幕现在的做法

见 `LiveCaptions.swift` 和 `MeetingCore/Utterances.swift`：

- Silero VAD 确认开口（`minSpeechDuration` 0.25 s）后，再等 0.4 s 才出第一条 partial；
- **每次 partial 都从头识别整句**：`Qwen3ASRModel.transcribe` 重新提取梅尔特征、重新编码整句、从第一个 token 开始贪心解码，没有任何复用；
- 句子越长，每次 partial 越贵，`PartialPacer` 会自动放慢刷新；落后实时时直接跳过 partial；
- 整句重识别，前面的字随时可能变，界面只能整行替换，所以看起来是“跳”出来的，不像一个字一个字地长出来；
- partial 路径上，音频 ≥ 1.5 s 时每次还会跑一遍 WeSpeaker 声纹，进一步占用 MLX 时间。

### 8.3 Qwen3-ASR 能借用哪些做法

Qwen3-ASR 的编码器先按 1 s 一块独立做卷积，再在 8 s 窗口内做块对角注意力（speech-swift `AudioEncoder.swift`：`nWindow` 50，`nWindowInfer` 800）。所以：**已经填满的 8 s 窗口，编码结果不会再变，可以缓存；当前窗口内是双向注意力，新音频会改变整窗的编码。** 这和 Foresight 的因果流式编码器不同，音频 KV 缓存无法像 Foresight 那样逐块追加。

社区的 C 实现 antirez/qwen-asr 默认的流式参数是：2 s 一块、8 s 编码窗口、**回退 5 个 token**、最后 2 块视为未定；只输出稳定的文字，最后一块再补齐剩余部分。思路与 Foresight 的 backoff 一致。Qwen 官方仓库的流式接口和参数名还没有核对源码。

按收益从高到低排列：

1. **前缀回退解码（最关键）。** 记住上一次 partial 的 token；下一次 partial 照常编码音频，再把“上次结果去掉末尾 k 个 token”（k ≈ 5）接在 `<asr_text>` 后面一次性预填，然后继续贪心解码到结束。
   - 稳定前缀不再闪动，只有末尾几个字会被改写；
   - 解码步数从整句降到 k 加新增字数，长句也能保持约 0.3 s 刷新；
   - 定稿时仍然整句不带前缀重新解码，保证准确率（相当于 Foresight 的 `final_prefill_disabled`）。

   speech-swift 0.0.29 的 `generateText` 不支持传入强制前缀，需要在本地给 speech-swift 打补丁或 fork（它锁定在 exact 版本，不影响 `mlx-swift-lm` 3.31.4 的锁定）。**具体改法尚未验证。**
2. **在停顿处出字。** 除了固定间隔，检测到约 150–200 ms 的短停顿（低于 0.5 s 断句静音）时立即出一次 partial，这样出字多落在词边界上。
3. **缩短首字延迟。**
   - 第一条 partial 从“确认开口 + 0.4 s”提前到“确认开口 + 0.2–0.25 s”；
   - partial 路径不再计算声纹，说话人留到定稿时再定；
   - partial 和 final 在 `MLXGate` 里排队时，final 优先（现在落后时只会跳过 partial）。
4. **界面区分稳定文字与未定文字。** `caption/partial` 增加 `stableChars`（稳定前缀的长度），界面把稳定部分正常显示，末尾未定的字用浅色显示，新字直接接在后面，不再整行替换。这要同时修改 `crates/mm-engine/src/methods.rs`、`EngineKit/Methods.swift`、`docs/engine-protocol.md` 和 GPUI 字幕视图。
5. **编码器窗口缓存（低优先）。** 句子目前最长 12 s，最多只能省掉一个 8 s 窗口的编码。只有将来放宽切句长度时才值得做。

不建议为了实时性换模型：真正的流式模型里，Nemotron 3.5 的中文明显较差（见第 3 节）。sherpa-onnx 的流式 Paraformer 中文尚可，可以作为“草稿层”，先出字再由 Qwen3-ASR 覆盖，但这样要多维护一套模型和运行时，而且没有实测，所以排在前缀回退之后。

**验证方法。** 扩展 `scripts/eval-captions.py`，在现有指标之外增加：

- 每个字从说出到显示的延迟（用合成语音的分词时间戳）；
- 已显示的字后来又被改掉的比例（闪动率）；
- 每次 partial 的解码耗时随句长的变化。

先只做第 1、3 项，比较 k = 3、5、8 下的闪动率和定稿字错误率，再决定是否修改协议和界面。

### 8.4 实测与落地（2026-11-08）

**Foresight 原版效果**（用 computer use 驱动 `/Applications/Foresight.app`，用 `say` 从扬声器播放测试语音）：

- 正在说的句子以**灰色斜体**显示，随说话逐词变长；说完后变成黑色定稿。肉眼看是一个词一个词往后长，前面的词基本不动，和 8.1 节的推断一致。
- 麦克风和系统声音是**两路独立转写**，同一段扬声器声音会出两份（一份 “Mac”，一份 “map”）。
- 两句之间停顿短时，会合成一个约 20 秒的长段，定稿要等整段说完。
- **TinyGemmaAsr 只支持英文。** 普通话（婷婷，约 20 秒）只出了一条 “..!!.” 的临时结果，没有任何定稿；西班牙语（Paulina）和日语（Kyoko）也只有 “ster” 之类的碎片。中文会议用不了，我们继续用 Qwen3-ASR，只借鉴它的调度方法。

**本项目落地的改动**（引擎 `PrefixDecoder.swift`、`MeetingCore/PrefixRollback.swift`、`Utterances.swift`；协议见 `engine-protocol.md` › 实时字幕）：

1. **前缀回退解码。** 直接调用 speech-swift 公开的特征提取、编码器、解码器和分词器，自己拼提示词：上一条 partial 去掉末尾 5 个 token 后接在 `<asr_text>` 后面，和音频一起预填，只往后解码。不需要 fork speech-swift，也不影响版本锁定。定稿仍然整句从头识别。
2. **停顿触发。** 说话中出现 ≥ 0.15 s 的词间停顿时立即出一次 partial，每个停顿一次。
3. **首字更早。** VAD 确认开口后 0.25 s 出第一条（原来是 0.4 s）；刷新下限从 0.3 s 降到 0.2 s；字幕循环每 50 ms 一轮（原来 100 ms）；partial 的说话人每句只估计一次。
4. **界面。** `caption/partial` 新增 `stableChars`；主窗口和浮窗把稳定部分正常显示，末尾未定的字用浅色显示，新字接在后面。

**测评。** `scripts/eval-captions.py`，Release 引擎，M5 Pro，合成语音 68 秒 12 句。两组都用了第 2、3 项的新调度，区别只在 partial 的解码方式（`MM_PARTIAL_PREFIX=0` 关闭前缀回退）。新增“改字率”指标：partial 已显示的字被下一条 partial 或定稿改掉的比例。

| 指标 | 整句重解码 | 前缀回退 |
| --- | --- | --- |
| partial 解码耗时（中位 / p90） | 109 / 216 ms | **81 / 116 ms** |
| partial 刷新间隔（中位 / p90） | 300 / 500 ms | **209 / 306 ms** |
| 第一条字幕（开口 → 显示，中位 / p90） | 791 / 1086 ms | **702** / 1105 ms |
| 句末到显示最后几个字（中位 / p90） | −25 / 152 ms | **−179 / −68 ms** |
| 改字率 | 2.1% | **1.4%** |
| 定稿延迟（中位） | 1164 ms | 1144 ms |
| 字错误率 | 0 | 0 |

和第 6 节（今天之前）相比，partial 刷新间隔中位从 330 ms 降到 209 ms，句子说完前约 0.18 s 屏幕上就已经出现最后几个字。p90 解码耗时几乎减半，说明长句不再拖慢刷新。合成语音很干净，改字率和字错误率的绝对值都偏低，真实会议要用真人录音复测。

**还没做到的。** 第一条字幕仍然约 0.7 s，主要卡在 Silero VAD 确认开口（≥ 0.25 s 语音）和第一次解码上。要再快，就得在 VAD 确认前先用低阈值试解码；这样容易对噪声出假字，暂时没做。在真实 App 里用 computer use 看到中文字幕边说边出、定稿正常，但截图没能抓到“浅色未定尾巴”正在显示的那一刻，这一点只有单元测试和协议数据可以证明。

## 9. TinyGemmaAsr 的中文替代：小型流式模型实测（2026-11-08）

**目标。** 找一个和 TinyGemmaAsr 同一定位的模型：小（约 200 MB 以内）、真正流式（逐块出字、已出的字基本不改）、中文好、在 Mac 上便宜到可以一直跑。

**测试集。**
- AISHELL-1 测试集随机 200 句（朗读，991 s）；
- WenetSpeech test_meeting 随机 200 句（真实会议，≥ 1.5 s，1335 s）。

**方法。**
- 字错误率：转简体，阿拉伯数字转中文，去掉标点和空格后逐字比较。
- 流式模型按 100 ms 一块喂音频，和实时字幕一样；计时是全部计算时间，RTF = 计算时间 / 音频时长。“单块最慢”是每 100 ms 音频的最大计算时间（p99），决定会不会卡顿；“partial 改字率”是已显示的字被后续 partial 改掉的比例。
- 非流式模型整句识别一次。
- sherpa-onnx（ONNX Runtime）跑 CPU 2 线程，Fun-ASR-Nano 用 4 线程；MLX 跑 GPU。机器为 M5 Pro。

| 模型 | 大小 | 流式 | AISHELL-1 | 会议集 | RTF | 单块最慢 | partial 改字率 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Qwen3-ASR 1.7B 4bit（现用，MLX） | 1.5 GB | 伪流式 | **3.84** | **6.19** | 0.065 | – | – |
| Fun-ASR-Nano int8（sherpa，CPU） | 0.84 GB | 否（LLM 解码） | 4.19 | 7.09 | 0.06–0.075 | – | – |
| Qwen3-ASR 0.6B 4bit（MLX） | 0.7 GB | 伪流式 | 4.19 | 7.87 | 0.024 | – | – |
| **流式 Zipformer 中文 int8**（sherpa） | **167 MB** | **是，320 ms 一块** | 4.65 | 9.85 | 0.045 | 17 ms | 0.3% |
| 流式 Zipformer 中文 xlarge int8 | 746 MB | 是，320 ms | 4.33 | 8.86 | 0.10 | 39 ms | 0.3% |
| SenseVoice Small int8（sherpa） | 247 MB | 否 | 5.00 | 10.81 | 0.022 | – | – |
| Cohere Transcribe 2B（MLX bf16） | 约 4 GB | 否 | 6.02 | 10.96 | 0.027 | – | – |
| X-ASR 中英 160 ms int8（带标点） | 175 MB | 是，160 ms | 6.27 | 11.80 | 0.070 | 12 ms | 0 |
| X-ASR 中英 480 ms int8（带标点） | 175 MB | 是，480 ms | 7.11 | 11.85 | 0.033 | 17 ms | 0 |
| 流式 Paraformer 中英 int8 | 230 MB | 是，600 ms | 5.95 | 14.45 | 0.037 | 23 ms | 0 |
| Moonshine base 中文 | 144 MB | 否 | 8.24 | 23.02 | 0.018–0.027 | – | – |
| 流式 Zipformer small CTC 中文 int8 | 26 MB | 是 | 8.87 | 22.40 | 0.015 | 9 ms | 0 |

**排除的两个指定模型。**
- **Cohere Transcribe（`CohereLabs/cohere-transcribe-03-2026`）。** 2B、离线，不流式。中文明显不如 Qwen3-ASR 0.6B（会议集 10.96% 对 7.87%），体积是后者的近 6 倍。它是英文第一（Open ASR Leaderboard），但不适合中文会议。另外 `mlx-community/cohere-transcribe-03-2026-mlx-8bit` 在 mlx-audio 2.0.1 里只输出乱码，要用官方仓库（需在 Hugging Face 同意协议）。
- **Kroko ASR（`Banafo/Kroko-ASR`）。** 结构上正是我们要的（sherpa-onnx 流式 Zipformer，约 156 MB），但**没有中文模型**，只有德、英、西、法、意等欧洲语言加希伯来语和土耳其语，所以没有实测。

**结论：TinyGemmaAsr 的中文替代选流式 Zipformer 中文 int8**（`sherpa-onnx-streaming-zipformer-zh-int8-2025-06-30`，源自 icefall `multi_zh-hans` zipformer-large，Apache-2.0）。理由：

- 定位一致：167 MB（TinyGemmaAsr 约 178 MB），真正流式，每 320 ms 一块（加约 130 ms 前瞻）；
- 便宜：CPU 2 线程 RTF 0.045，每块最多 17 ms，完全不占 GPU，不和 Qwen3-ASR 抢 MLX；
- 稳：partial 改字率 0.1–0.3%，字基本只往后长，正是“边说边出字”要的效果；
- 准确率在真正流式的模型里最好：会议集 9.85%，比同大小的 X-ASR（11.8%）、流式 Paraformer（14.45%）都好。xlarge 版好 1 个点，但体积 4.5 倍、计算 2 倍，不划算。

**但它不能单独替代 Qwen3-ASR。** 会议集 9.85% 对 6.19%，差 3.7 个点，所以要像 Foresight / Google 的两段式那样用：

1. **草稿层：** 流式 Zipformer 每 320 ms 出一次字，直接显示，几乎不改字；
2. **定稿层：** VAD 判定说完后，仍由 Qwen3-ASR 1.7B 整句识别，覆盖草稿。

这样首字延迟只取决于 Zipformer 的分块（按 320 ms 分块加前瞻推算约 0.45 s，尚未在实时管线里实测；不再等 VAD 确认和第一次 LLM 解码），GPU 只在定稿时用。代价是定稿时会改掉草稿里约 3–4% 的字，以及多一个 ONNX Runtime 依赖（sherpa-onnx 有 Swift / C API 和 macOS arm64 预编译库）。

**其他发现。**
- **Fun-ASR-Nano** 是最接近 Qwen3-ASR 1.7B 的小模型（会议集 7.09%），但它是 LLM 解码、不流式，CPU 上长句要 1.4 s。适合做定稿或精转的备选，不适合草稿层。
- **Qwen3-ASR 0.6B** 在准确率上胜过所有流式小模型（会议集 7.87%），GPU 计算只有 1.7B 的 37%。如果不想加 sherpa-onnx 依赖，可以只把 partial 换成 0.6B（配合第 8 节的前缀回退），定稿仍用 1.7B。
- **X-ASR 中英** 是唯一能处理中英夹杂并自带标点的小型流式模型，但中文比 Zipformer 差约 2 个点。中英混说多的会议可以考虑。
- 合成语音和朗读语音都偏干净；会议集是真实录音，但每句是人工切好的，不含 VAD 切分误差。

## 资料来源

- Qwen3-ASR 官方仓库与技术报告：https://github.com/QwenLM/Qwen3-ASR
- Nemotron 3.5 ASR Streaming 模型卡：https://huggingface.co/nvidia/nemotron-3.5-asr-streaming-0.6b
- Nemotron 3.5 MLX 移植与 M2 Max 实测：https://huggingface.co/Mikhailo/nemotron-3.5-asr-streaming-0.6b-mlx-fp16
- Voxtral Mini 4B Realtime 模型卡：https://huggingface.co/mistralai/Voxtral-Mini-4B-Realtime-2602
- Voxtral Realtime 8-bit MLX 实测：https://huggingface.co/flo5k5/Voxtral-Mini-4B-Realtime-2602-MLX-8bit
- speech-swift 模型列表：https://github.com/soniqo/speech-swift
- funasr-mlx：https://github.com/baicai-1145/funasr-mlx
- mlx-audio Fun-ASR-Nano：https://blaizzy.github.io/mlx-audio/models/stt/fun-asr-nano/
- 本地模型对比（含 Nemotron 中文数据转引）：https://www.voconly.com/en/blog/local-ai-asr/local-asr-model-comparison-2026/
- Open ASR Leaderboard 2026-10 快照：https://apidog.com/blog/best-open-source-speech-to-text-models/
- FireRedASR2S 仓库与评测表：https://github.com/FireRedTeam/FireRedASR2S
- FireRedASR2S 技术报告：https://arxiv.org/abs/2603.10420
- FireRedASR2-AED MLX 实现：https://github.com/Blaizzy/mlx-audio/tree/main/mlx_audio/stt/models/fireredasr2
- sherpa-onnx FireRedASR2 模型：https://k2-fsa.github.io/sherpa/onnx/FireRedAsr/index.html
- MiMo-V2.5-ASR 博客（含 Qwen3-ASR、Fun-ASR v1.5、Seed-ASR 2.0、Gemini 3.1 Pro 对比）：https://mimo.xiaomi.com/blog/mimo-v2-5-asr
- MiMo-V2.5-ASR MLX 移植：https://huggingface.co/mlx-community/MiMo-V2.5-ASR-MLX-8bit
- VibeVoice-ASR 模型卡：https://huggingface.co/microsoft/VibeVoice-ASR
- VibeVoice-ASR 在 Mac 上的实测：https://simonwillison.net/2026/Apr/27/vibevoice/
- VibeVoice-ASR 长音频重复问题：https://github.com/microsoft/VibeVoice/issues/373
- MOSS-Transcribe-Diarize 模型卡：https://huggingface.co/OpenMOSS-Team/MOSS-Transcribe-Diarize
- MOSS-Transcribe-Diarize 技术报告：https://arxiv.org/abs/2601.01554
- speech-swift MOSS MLX 测评：https://github.com/soniqo/speech-swift/blob/main/docs/benchmarks/moss-mlx.md
- Fun-ASR v1.5 介绍：https://qwenaudio.github.io/funasr/
- 多教师蒸馏论文（Qwen3-ASR 会议集另一组数字）：https://arxiv.org/abs/2608.03610
- Cohere Transcribe 模型卡：https://huggingface.co/CohereLabs/cohere-transcribe-03-2026
- Kroko ASR 模型列表：https://huggingface.co/Banafo/Kroko-ASR
- sherpa-onnx 流式 Zipformer 中文：https://huggingface.co/csukuangfj/sherpa-onnx-streaming-zipformer-zh-int8-2025-06-30（训练结果：https://github.com/k2-fsa/icefall/blob/master/egs/multi_zh-hans/ASR/RESULTS.md）
- X-ASR 中英流式：https://huggingface.co/GilgameshWind/X-ASR-zh-en
- sherpa-onnx 模型发布页：https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models
- Fun-ASR-Nano：https://huggingface.co/FunAudioLLM/Fun-ASR-Nano-2512
- SenseVoice Small：https://huggingface.co/FunAudioLLM/SenseVoiceSmall
- 测试集：AISHELL-1 测试集 https://huggingface.co/datasets/AudioLLMs/aishell_1_zh_test；WenetSpeech test_meeting https://huggingface.co/datasets/TwinkStart/WenetSpeech
- Qwen3-ASR 技术报告（1–8 s 动态注意力窗口，支持流式）：https://arxiv.org/abs/2601.21337
- antirez/qwen-asr（C 实现，流式默认 2 s 分块、回退 5 token）：https://github.com/antirez/qwen-asr
- Foresight 拆解：本机 `/Applications/Foresight.app` 1.0.0 的二进制字符串与模型目录（2026-11-08）
