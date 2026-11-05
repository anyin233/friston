# 设计：iPhone 录音笔与 Mac 的互联（不依赖局域网）

前提（见 [ios-feasibility.md](ios-feasibility.md)）：iPhone 是轻量客户端，既能独立当录音笔，也能在 Mac 旁边充当空间麦克风。本文回答一个问题：两者怎么连，才能不受酒店、公司访客网络常见的 **AP 隔离**（同一 Wi-Fi 下的设备互相不可见）影响。

本机情况（2026-11-05）：iPhone 是 **iPhone 17 Pro**（Goldenglow，iPhone18,1），当前未连接。Mac 上已有 `awdl0`（Apple 点对点 Wi-Fi）、`nan0`（Wi-Fi Aware 的 NAN 接口）和 `/var/run/usbmuxd`（USB 多路复用服务）。

## 1. 核心思路：手机本地录音是唯一真相，链路只是加速

不要让任何一条链路成为单点故障：

- 手机**始终在本地完整录音**：单声道语音 + 每帧方位线索，可选保留 FOA 原始声场。
- 和 Mac 连上时，额外推一路**实时流**，让 Mac 立刻出字幕、做说话人融合。实时流尽力而为，断了不影响录音。
- 事后由**文件同步**补齐：断线期间的片段、FOA 原始数据、手机上生成的字幕都以手机文件为准。

这样，"切换链路"和"断线重连"就不再是难题：每一帧带序号，手机缓存最近几分钟；重连后从断点续传，超出缓存的部分交给文件同步。

## 2. 带宽预算：实时流其实很小

方位线索在手机上算（MeetingCore 已验证能编译到 iOS），所以 Mac 不需要原始 FOA：

| 数据 | 码率 | 说明 |
| --- | --- | --- |
| 单声道语音（AAC-ELD，16 kHz） | 约 24–32 kbps | 供 Mac 跑 ASR 和声纹 |
| 方位线索（每 100 ms：角度、置信度、能量） | 约 1–2 kbps | 直接进 `SpatialCue` |
| 控制消息（开始/停止、标记、时钟校准） | 可忽略 | |
| **实时合计** | **约 35 kbps** | |
| FOA 原始 4 声道 PCM（48 kHz/16 bit） | 约 6 Mbps，约 1.4 GB/小时 | 只走文件同步，而且只走快链路 |

实时流约 35 kbps，**连蓝牙都能扛**。只有批量同步才需要快链路。

## 3. 可选链路

| 链路 | 受 AP 隔离影响 | 实时流 | 批量同步 | 公开接口 | 风险 |
| --- | --- | --- | --- | --- | --- |
| **USB-C 线**（usbmuxd 隧道） | 不受影响 | ✅ | ✅✅ 最快 | iOS 端：普通 TCP 监听；Mac 端：**非公开**的 usbmuxd 协议 | 协议未公开（PeerTalk、libimobiledevice、Xcode 都在用，多年稳定）；Mac App 若进 App Store 沙盒，需要临时例外权限 |
| **Apple 点对点 Wi-Fi**（AWDL，Network.framework `includePeerToPeer`） | 原理上不受影响：设备之间直连，不经过路由器（AirDrop 用的就是它） | ✅ | ✅ | 公开 | 共用一个 Wi-Fi 射频，同时连着路由器时会分时，吞吐下降；论坛有 iOS 26 上 AWDL 异常的报告 |
| **蓝牙 BLE L2CAP**（Core Bluetooth） | 不受影响 | ✅（35 kbps ≪ 实测 300–600 kbps） | ❌ 太慢（1 小时压缩 FOA 要传约 50 分钟） | 公开 | 吞吐不稳，连接参数由系统决定 |
| 同一局域网 Bonjour | **受影响** | ✅ | ✅ | 公开 | AP 隔离、跨 VLAN 时直接失效 |
| Wi-Fi Aware（iOS 26） | 不受影响 | — | — | 公开，但**只有 iOS/iPadOS**，macOS 没有接口 | Mac 端用不了（`nan0` 存在，但没有公开 API） |

依据：
- AWDL：[peerToPeerIncluded](https://developer.apple.com/documentation/network/nwparametersprovider/peertopeerincluded(_:).md)。DTS 的说法是，不在同一网络接口上的两台设备要靠点对点 Wi-Fi 来连（[论坛 790725](https://developer.apple.com/forums/thread/790725)）。iOS 26 上的异常报告见 [论坛 808917](https://developer.apple.com/forums/thread/808917)。
- Wi-Fi Aware 只支持 iOS/iPadOS 26（[WiFiAware](https://developer.apple.com/documentation/wifiaware.md)、[WWDC25 session 228](https://developer.apple.com/videos/play/wwdc2025/228)）。
- BLE 实测吞吐：DTS 给出的最好情况约 400–600 kbps，并且不稳定（[论坛 770717](https://developer.apple.com/forums/thread/770717)）。
- USB：[PeerTalk](https://docs.rs/crate/peertalk/latest)，沙盒限制见其说明。

## 4. 推荐组合：蓝牙打底 + 有线优先 + 点对点 Wi-Fi 补充

```
            ┌──────── 控制 + 实时流（始终可用） ───────┐
iPhone ─────┤  BLE L2CAP   ~35 kbps，不依赖任何网络      ├───── Mac
 本地录音    │                                          │   mm-meetingd
 方位线索    ├──────── 快链路（有就用，自动升级） ───────┤
            │  ① USB-C 线（插上即用，还能充电）          │
            │  ② Apple 点对点 Wi-Fi（AWDL）              │
            │  ③ 同局域网 Bonjour（没隔离时）             │
            └──────────────────────────────────────────┘
```

- **蓝牙负责"永远在线"**：发现、唤醒、开始/停止、实时流和时钟校准都走它，完全绕开 Wi-Fi 和路由器。iOS 后台录音时可以继续工作（需要 `bluetooth-peripheral` 和 `audio` 两个后台模式）。
- **插上线是最好的体验**：USB-C 线同时供电、传数据。前面担心的长时间录音发热、耗电，在桌边插线时也一并解决了。插线后实时流切到 USB，并自动开始同步 FOA 文件。
- **点对点 Wi-Fi 负责无线时的批量同步**：会后不插线也能把文件传过去。
- 链路选择按"可用就升级、断开就降级"进行。上层只看到一条逻辑会话。

## 5. 会话层（与链路无关）

所有链路之上跑同一套会话层：

1. **配对一次**：Mac 显示二维码（含 Mac 的公钥和一次性口令），手机扫码。双方用 CryptoKit 的 X25519 交换并保存长期密钥。之后不再需要输入任何东西。
2. **每次连接**：基于长期密钥做一次握手（X25519 + HKDF），协商出会话密钥，所有帧用 ChaChaPoly 加密。在应用层统一加密，所以蓝牙、USB、Wi-Fi 的安全性一致，不依赖各链路自带的加密。
3. **分帧**：每帧有长度前缀，再加类型、序号和时间戳。控制消息沿用引擎协议的 JSON-RPC 格式，音频和线索走二进制帧。新协议名字要写进 `docs/engine-protocol.md`，并同步修改 `methods.rs` 和 `Methods.swift`。
4. **时间对齐**：
   - 粗对齐：链路上做 NTP 式往返测量。蓝牙往返几十毫秒，足够满足 100 ms 级的线索对齐。
   - 精对齐：如果 Mac 自己也在录音，就把手机的单声道和 Mac 的麦克风做互相关，直接得到偏移和漂移，比任何网络对时都准。
5. **回声归属**：Mac 外放的远程声音会被手机录进去。对齐后把手机音频与 Mac 的系统声音做互相关，相关度高的片段就标为"系统声音"来源，不当作会议室里的人。

## 6. 权限与上架

- iOS：用 Bonjour 和点对点 Wi-Fi 需要 `NSLocalNetworkUsageDescription` 和 `NSBonjourServices`；用蓝牙需要 `NSBluetoothAlwaysUsageDescription`；还要开启后台模式 `audio` 和 `bluetooth-peripheral`。
- macOS：本地网络隐私同样需要用途说明；还需要蓝牙权限。usbmuxd 套接字对非沙盒 App 可以直接访问（本机权限为 `srw-rw-rw-`）。

## 7. 待实测

1. **USB**：iPhone 17 Pro 插上 Mac 后，Mac 能否通过 usbmuxd 连到手机 App 监听的端口；连续 1 小时传输的吞吐和稳定性。
2. **AP 隔离**：两台设备连同一个开了 AP 隔离的 Wi-Fi（或者手机不连任何 Wi-Fi），点对点 Wi-Fi 能否发现并连上；对比 Mac 同时连着 5 GHz 路由器时的吞吐。
3. **蓝牙**：手机锁屏录音时，L2CAP 能否稳定传 35 kbps，延迟分布如何。
4. **切换**：拔线、关 Wi-Fi、走出蓝牙范围后，会话能否无缝降级和续传，数据是否零丢失。
