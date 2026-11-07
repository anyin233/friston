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
