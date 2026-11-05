# 可行性分析：iPhone App，全部逻辑在 iPhone 上运行

问题：Mac 内置麦克风做不了空间录音（见 [spatial-feasibility.md](spatial-feasibility.md) 第 4 节），而 iPhone 支持。那么能否做一个 iPhone App，把录音、实时字幕、说话人识别、索引和纪要**全部**放在手机上？

## 结论

**作为"会议室录音笔"可行；作为 Mac 版的完整替代不可行。** 卡住的不是算力，而是三条 iOS 系统规则：

1. **抓不到其他 App 的会议音频。** 用手机上的腾讯会议、Teams 开会时，我们的 App 通常拿不到麦克风，也拿不到对方 App 播放的声音。"流来源"这类证据（远程与本地硬分区）在手机上基本失效。
2. **锁屏或切到后台后不能用 GPU。** MLX 跑在 GPU 上，所以实时字幕要求 App 一直在前台、屏幕常亮。
3. **只能有一个进程。** Mac 上把两个 MLX 引擎拆成两个进程，是为了绕开依赖冲突。iOS App 不能启动子进程，这个冲突必须换一种方式解决。

推荐路线：**先做"iPhone 当空间麦克风 + 录音笔"，重活留给 Mac**（见最后一节），而不是全量移植。

## 逐项评估

| 模块 | Mac 版现状 | iPhone 上 | 判断 |
| --- | --- | --- | --- |
| 空间录音 | 内置麦只有 1 声道 | AVFoundation FOA 4 声道（iOS 18+，机型见下） | ✅ 这正是做 iPhone 版的理由 |
| 说话人融合、GCC-PHAT、立体声线索 | `NativeKit/MeetingCore` | 已实测可编译（见下） | ✅ 直接复用 |
| 实时字幕 Qwen3-ASR 0.6B | speech-swift + MLX | speech-swift 声明支持 iOS 18，MLX 支持 iOS 17 | ⚠️ 前台可行；后台不行；发热和续航要实测 |
| 分离与声纹 Sortformer + WeSpeaker | Core ML | Core ML 可用 | ⚠️ 后台能否跑在神经引擎上，未找到官方说明 |
| 会后精修 Qwen3-ASR 1.7B + 对齐 | MLX | 内存够，但要在前台跑完；iOS 26 可申请后台 GPU 权限，并非所有机型都支持 | ⚠️ 建议放回 Mac，或者充电时在前台跑 |
| EmbeddingGemma 2 多模态索引 | 独立进程 `mm-embedd` | 与 speech-swift 的 `mlx-swift-lm` 版本冲突，不能链接进同一个 App | ❌ 必须改方案（见下） |
| 窗口录制 ScreenCaptureKit | ✅ | iOS 没有。ReplayKit 广播扩展能录整屏和 App 声音，但扩展的内存很小，跑不了模型 | ❌ |
| 系统声音 / 远程参会者 | process tap | 同上；而且通话中通常拿不到麦克风 | ❌ |
| 日程 EventKit | ✅ | ✅ | ✅ |
| SQLite + sqlite-vec + FTS5 | Rust `mm-store` | Rust 可以编译到 iOS，再用 UniFFI 桥接；本机尚未安装 iOS 的 Rust 目标，未实测 | ⚠️ 工作量中等 |
| Obsidian 笔记 | 直接写库文件 | iOS 版 Obsidian 的库在"我的 iPhone"或 iCloud 里，可以通过文件选择器授权访问 | ⚠️ 可行，受管区块逻辑可复用 |
| 纪要：Codex app-server | 启动 `codex` 子进程 | 不能启动子进程 | ❌ 改用 BYOK HTTP，或 Apple 端侧基础模型（iOS 26+） |
| 界面 GPUI | Rust GPUI | GPUI 不支持 iOS | ❌ 用 SwiftUI 重写 |

### 已实测

- 把 `NativeKit` 复制到临时目录，加上 `.iOS("18.0")` 平台后，用 `xcodebuild -destination 'generic/platform=iOS'` 编译。`AudioInputProbe.swift` 报错，因为它用的是 macOS 独有的 Core Audio HAL 接口。用 `#if os(macOS)` 包起来后，**EngineKit 和 MeetingCore 全部编译通过**。仓库里的文件没有改动。
- speech-swift 0.0.29 的 `Package.swift` 声明了 `.iOS("18.0")`，mlx-swift 和 mlx-swift-lm 声明了 `.iOS(.v17)`。这只说明能编译，没有在真机上跑过模型。

### 三个硬限制的依据

1. **通话中录不了音。** 没有找到 Apple 的正式文档。开发者论坛和 Stack Overflow 一致反映：另一个 App 通话或来电时，录音会被中断，结束后恢复也不稳定（[论坛 784782](https://developer.apple.com/forums/thread/784782)、[论坛 120038](https://developer.apple.com/forums/thread/120038?page=3)）。按"不可用"做规划，真机上再确认一次。
2. **后台不能用 GPU。** Apple 文档说明，iOS 会限制后台 App 访问 GPU，后台提交的工作会被拒绝。长时间的后台 GPU 任务要申请 Background GPU Access 权限，配合 Background Tasks 使用，而且不是所有机型都支持（[Preparing your Metal app to run in the background](https://developer.apple.com/documentation/metal/preparing-your-metal-app-to-run-in-the-background)、[论坛 691805](https://developer.apple.com/forums/thread/691805)）。Core ML 在后台是否能跑在神经引擎上，没有找到说明。
3. **依赖冲突。** 项目规定 speech-swift 锁定 `mlx-swift-lm` 3.31.4，EmbeddingGemma 2 需要 main，二者不能合并（见 CLAUDE.md）。SwiftPM 在一个 App 里只能解析出一个版本。可选的办法：
   - 手机上不做多模态嵌入，索引留给 Mac（推荐）；
   - 换成能在 3.31.4 上运行、或基于 Core ML 的较小嵌入模型。代价是向量与 Mac 端的模型戳不一致，两端需要分开索引；
   - 等 speech-swift 升级到 main。

### 空间录音的机型

- Apple 支持文档列出，支持"语音备忘录"空间音频录制的机型是 iPhone 16、16 Plus、16 Pro、16 Pro Max、16e（[Apple 支持](https://support.apple.com/en-ie/guide/iphone/aside/iphdfdbfb480/ios)）。文档没有列出 17 系列，但新机型通常也支持，需要实测。
- 第三方 App 用的 `AVCaptureDeviceInput.multichannelAudioMode` 接口从 iOS 18 开始提供。有报道称 iPhone 15 Pro 也可用（[wccftech](https://wccftech.com/iphone-15-pro-models-will-finally-be-able-to-capture-spatial-audio-through-third-party-apps-in-ios-18/)），未经官方确认。运行时以 `isMultichannelAudioModeSupported(.firstOrderAmbisonics)` 为准。
- 用 AVAssetWriter 自行写入空间音频需要 iOS 26（[WWDC25 session 251](https://developer.apple.com/videos/play/wwdc2025/251/)）。我们只需要 `AVCaptureAudioDataOutput` 给出的 4 声道 PCM，再由自己计算方位。

**FOA 的价值和风险**：W/X/Y/Z 四个分量可以用声强向量 `θ = atan2(⟨W·Y⟩, ⟨W·X⟩)` 直接得到 360° 方位角，没有双麦的前后歧义。这与 `SpatialCue` 兼容，例如 `[cos θ, sin θ]`。风险在于这是系统用 3–4 个麦克风合成的声场，指向精度没有公开指标，需要实测。另外手机平放在桌上时，"水平方位"取决于摆放朝向，好在融合只依赖同一场会议内的相对位置，这一点不影响。

## 推荐路线

**阶段 1：iPhone 作为 Mac 的空间麦克风（工作量小，收益最大）**

- iPhone App 只负责：采集 FOA → 计算每帧的方位线索（复用 MeetingCore）→ 通过局域网（Network.framework / Bonjour）把单声道语音和方位线索推给 Mac 上的 `mm-meetingd`。
- Mac 继续负责其余一切：系统声音、窗口录制、ASR、声纹、索引、纪要。
- 需要解决两个问题：
  - **时钟对齐**：iPhone 和 Mac 的时间戳不同源。线索只用于给说话人片段投票，按 100 ms 级别对齐就够了，不需要样本级同步。
  - **回声**：Mac 外放的远程声音也会被 iPhone 录进去，要靠流来源和时间对齐把它排除。
- 新增的协议方法要同步修改 `methods.rs`、`Methods.swift` 和 `engine-protocol.md`。

**阶段 2：iPhone 独立录音笔（会议室面对面开会、没有 Mac 的场景）**

- 在前台录 FOA 加单声道混音，实时字幕用 0.6B 模型，说话人用 Sortformer + 声纹 + FOA 方位。整个过程屏幕常亮，并提示用户接上电源。
- 会后精修和索引默认推迟到下次与 Mac 同步时做。纪要优先用 BYOK HTTP，也可以选 Apple 端侧基础模型。
- 不支持：同一台手机上用会议 App 开会的同时录音。

**不推荐：全量移植。** 它要重写界面（SwiftUI），要解决依赖冲突，要用 UniFFI 桥接 Rust 存储层，还要替换 Codex。做完之后仍然录不到远程参会者，也做不了后台实时字幕。

## 下一步需要实测的

1. 你的 iPhone 机型上，`isMultichannelAudioModeSupported(.firstOrderAmbisonics)` 是否为 true，以及在一张会议桌上，FOA 声强向量方位的稳定性（可以沿用 spatial-feasibility.md 2.4 节的测试思路）。
2. Qwen3-ASR 0.6B 在 iPhone 前台连续跑 60 分钟的延迟、发热降频和耗电。
3. 另一个 App 通话时，本 App 能否继续录音（预期不能）。
4. Core ML 的 Sortformer 和 WeSpeaker 能否在锁屏后继续运行。
