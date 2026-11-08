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
