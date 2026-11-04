# 狼蛛 F87 / 接收器 3554:FA09 —— HID 实测笔记

2026-10-31 在本机（Windows 11）实测，键盘为狼蛛 F87，2.4G 模式，接收器 product
string 为 `2.4G Wireless Receiver`、厂商 `Compx`。

## 1. 接口

接收器是一个复合设备（`MI_00` 键盘 + `MI_01` 复合集合），共 7 个 HID 集合：

| 集合 | usage page / usage | 报文 | 说明 |
| --- | --- | --- | --- |
| `MI_01&COL01` | `0xFF02` / `0x02` | in 20 / out 20，report id `0x13` | **唯一控制通道** |
| `MI_01&COL06` | `0xFF04` / `0x02` | feature 8，report id `0x06` | 7 字节静态记录 |
| `MI_00`、`COL04` | `0x01` / `0x06` | 键盘 | 标准输入（boot / NKRO） |
| `COL02` | `0x0C` / `0x01` | 3 字节 | Consumer |
| `COL03` | `0x01` / `0x80` | 2 字节 | System Control |
| `COL05` | `0x01` / `0x02` | 8 字节 | 鼠标（接收器兼作鼠标接收） |

控制通道的打开方式与雷柏不同：用 `WriteFile` / `ReadFile`（中断传输），
不是 `HidD_SetOutputReport`；实测两者都能写成功，但读只能靠 `ReadFile`。
`HidD_GetInputReport` 在该集合上一律失败。

## 2. 帧格式（20 字节）

```
13  cmd  sub  seq  len  data[14]  sum      sum = bytes[0..19] 累加 & 0xFF
```

2.4G 下没有 520 字节 feature 报文，所有逻辑都塞进 20 字节分片；多分片消息用
`seq` 递增。校验和是首 19 字节的模 256 和。

## 3. 已验证命令

| cmd | 含义 | 应答 | 实测 |
| --- | --- | --- | --- |
| `0x44` | 读配置 | 10 帧 `13 44 0A seq len data`，合并为 128 字节配置块 | ✅ 21 ms 内返回 |
| `0x41` | 读另一块（128 字节级） | 10 帧 `13 41 24 …` | ✅ |
| `0x42` | 读逐键映射 | 27 帧 `13 42 1B …` | ✅ |
| `0x43` | 读灯效调色板 | 37 帧 `13 43 25 …` | ✅ |
| `0x49` | 读未知块（35 帧，内容多为 `FF`） | `13 49 23 …` | ✅ |
| `0x0A` | 保存 / 状态推送 | 单帧 `13 0A 01 00 04 02 01 00 00 …` | ✅ 按键或插拔线时主动推送 |
| `0x05` | 状态推送 | `13 05 01 00 0A 03 00 00 00 00 8F 01 …` | 偶发，一次 |

配置块 128 字节的关键偏移（与社区文档一致，实测吻合）：

| 偏移 | 含义 | 实测值 |
| --- | --- | --- |
| 9 | 逐键模式标志 | `00` |
| 10 | 当前灯效 | `0F`（15，旋转风车） |
| 12 | 色彩模式 | `02` |
| 18 | 侧灯效果 | `00` |
| 24 | 休眠时间，30 s 为单位，0=不休眠 | `02`（1 分钟） |
| 56 + 2n | 灯效 n 的亮度 | — |
| 57 + 2n | 灯效 n 的速度（高 4 位）+ 颜色槽（低 4 位） | — |
| 126–127 | 结束标记 `5A A5` | ✅ |

## 4. 电量：2.4G 下拿不到

这是本次重点排查项，结论是**该固件不提供**，证据如下：

1. **`0x4A` 无应答。** 发送 `13 4A 00 … 5D`（社区在 F75 / F87 Pro 上验证过的电量
   查询帧），6 次重试、每次监听 1.2 s，**连回声都没有**。
2. **不是休眠导致的。** 同一次会话里先发 `13 44 01`（读配置，21 ms 收到 10 帧），
   紧接着发 `0x4A`，仍然 0 帧。链路明确是活的。
3. **不是插线导致的。** 充电中与拔线后各测一轮，两者都是 0 帧。
4. **没有主动上报。** 静默监听 45 s，0 帧；静默 35 s + 插回充电线，只收到
   `0x0A` 状态帧，没有电量帧。
5. **`Fn+B` 不会产生厂商报文。** 按住 Fn+B 期间静默监听 40 s，0 帧——电量灯效是
   键盘本地逻辑。
6. **`0xFF04` 的 feature 通道不是电量。** 读回恒为 `06 00 02 64 64 64 95 4C`；
   充电与拔线两种状态完全一致；`SetFeature` 只有 byte1 可写（写什么读回什么），
   byte2–7 只读。曾写 `0x4A` 试探，读回变成 `06 4A 02 …` 但数据区无变化，已写回
   `0x00` 复原。

旁证：官方说明与评测都指出 F87 / F87 Pro 的电量只能用 `Fn+B` 看氛围灯条（5 段 =
20% 一档），想要百分比得切到蓝牙模式由系统读标准 HID 电池；PeriphMonitor 也记录
`3554:FA09` 的 F99 Pro「write 可达但设备不回复」。

> 注：`aula-f87-control` 项目声称在 F87 Pro 上 `0x4A` 可用（✅）。本机这批
> F87 固件不响应，说明该命令是**按固件版本/型号裁剪**的，不能假定通用。

## 5. 官方驱动（AULA HUB 2.5.3）分析

- 安装位置：`%LOCALAPPDATA%\Programs\Aula Hub`，Electron 应用，前端是本地 SPA
  （`resources/app.asar` 里的 `dist/`），设备通信走 **WebHID**：
  `navigator.hid.requestDevice({filters:[{vendorId:0x3554, productId:0xfa09, usagePage:0xff02, usage:2}]})`。
- 所以它能"看到设备"（本地枚举），但页面卡在 logo：它依赖的
  `hub.aulacn.com` / `app.aulacn.com` 实测返回 **403 Forbidden**（阿里云 ESA 拦截，
  返回 110 字节的重定向桩），前端拿不到数据就一直停在加载态。
- asar 内部代码做过 javascript-obfuscator 混淆，字符串全在解码表里；可以看到一个
  命令枚举 `{EncryptionData:1, PCDriverStatus:2, DeviceOnLine:3, BatteryLevel:4,
  DongleEnterPair:5, …Set4KDongleRGB:0x14}` —— 这是给 4K dongle / 鼠标一类设备的
  驱动级协议，**不是** 0x13 的 20 字节通道，和 F87 电量无关。

## 6. 两个容易踩的坑

1. **键盘空闲约 60 秒就休眠，休眠时任何命令都不回**（连回声都没有）。零散测试会
   表现成"时好时坏"，必须先敲一下键盘唤醒，再用"读配置有应答"证明链路活着，
   然后才判断某条命令是否真的不支持。
2. 接收器的 vendor 通道写用 `WriteFile`（中断 OUT），读用 `ReadFile`（可 overlapped）；
   `HidD_GetInputReport` 在这个集合上永远失败，别拿它判断"设备没数据"。

## 7. 复现方法

```bash
# 枚举所有集合、caps、描述符，并批量读 feature
cargo run --example probe -- 3554

# 读配置并解析
cargo run --example f87_battery -- --config

# 电量查询（带 6 次重试、全量打印回帧）
cargo run --example f87_battery -- --battery

# 扫描命令族：0x40-0x4F / 0x50-0x5F
cargo run --example f87_battery -- --fuzz
cargo run --example f87_battery -- --scan 50-5f

# 静默监听（看有没有主动上报）
cargo run --example f87_battery -- --idle 30

# 任意帧
cargo run --example f87_battery -- --send "13 44 01 00"
```

`--fuzz` 会先把 128 字节配置快照写到 `target/f87_config_before.txt`，扫描完再读一次
对比，防止误写。

## 8. 还能做什么

- 灯效 / 亮度 / 速度 / 休眠 / 侧灯 / 逐键映射的读写：协议已通，`0x04`/`0x09`/`0x02`
  写、`0x0A` 保存，社区文档齐全。
- 电量：只能走蓝牙模式（系统标准 HID 电池），或让用户在键盘上看 `Fn+B`。
