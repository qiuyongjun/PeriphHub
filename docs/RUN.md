# PeriphHub 运行说明

PeriphHub 是一个 Windows 桌面外设配置面板，直接用 USB HID 厂商协议读写鼠标/键盘参数，
不依赖任何厂商驱动或后台服务。本文覆盖：环境准备、开发运行、打包、首次启动行为、
配套诊断工具，以及常见的踩坑排查。

## 最短路径

```powershell
cd D:\Project\PeriphHub
cargo run                                              # 直接跑（需 Windows + Rust 工具链）
$env:PERIPHHUB_DEMO=1; cargo run                       # 手边没设备，看界面用
cargo build --release                                  # 出单文件 exe：target\release\PeriphHub.exe
```

三条命令跑通就不用往下看了；设备识别、托盘、自启动这些细节见后面几节。

---

## 1. 运行环境

| 项目 | 要求 |
| --- | --- |
| 操作系统 | Windows 10 / 11（x64）。代码直接调用 SetupAPI、`hid.dll`、注册表，**只能跑在 Windows 上** |
| Rust | stable 工具链，MSVC 目标（本机验证版本：`cargo 1.96.0` / `rustc 1.96.0`） |
| 构建工具 | Visual Studio 生成工具 + "使用 C++ 的桌面开发"（提供 `link.exe`） |
| 图形 | 显卡驱动支持 OpenGL 3.x（界面用 eframe + glow 渲染） |
| 权限 | **不需要管理员**。HID 用共享模式打开，自启动写的是当前用户的 `HKCU` |

还没装 Rust 的话：

```powershell
winget install Rustlang.Rustup
rustup default stable-x86_64-pc-windows-msvc
```

装完开个新终端，`rustc -V` 能出版本号即可。

---

## 2. 开发运行（最常用）

在仓库根目录（`Cargo.toml` 所在目录）执行：

```powershell
cd D:\Project\PeriphHub
cargo run
```

- 首次编译要拉 eframe / winit / windows-sys 等依赖，**约 3–10 分钟**；之后增量编译几秒。
- 探针/调试版会额外弹一个控制台窗口，这是正常的：`main.rs` 里的
  `#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` 只在 release 下隐藏控制台。
- 启动后默认是**桌面浮窗**（296 宽、右下角起手），深色主题（`ThemeMode` 默认 `Dark`）；
  点浮窗右上角的"打开主界面"展开 960×680 的工作台，标题栏右侧有主题切换、设置、关闭。

只要编译通过、窗口能出来，硬件没插也能跑——只是设备列表为空。

---

## 3. 没有设备时：演示模式

演示模式会注入 4 台虚拟的雷柏鼠标（VT1 Pro / VT3 Pro / VT9 Pro / VT1 Pro Max，覆盖 2.4G、蓝牙、
有线三种连接，电量分别为 86%、34%、12%，有线那台不显示电量），用来调界面或截图：

```powershell
# PowerShell
$env:PERIPHHUB_DEMO=1; cargo run
```

```bat
:: cmd
set PERIPHHUB_DEMO=1 && cargo run
```

```bash
# Git Bash
PERIPHHUB_DEMO=1 cargo run
```

判断依据是 `src/devices.rs` 里读环境变量 `PERIPHHUB_DEMO`，值为 `1`（或 `true`，不区分大小写）即生效；
生效时首页标题旁会出现"演示数据"标签。演示模式不会打开任何真实 HID 句柄，可以和真机面板同时开着。

---

## 4. 打包发布版

```powershell
cargo build --release
```

产物：`target\release\PeriphHub.exe`（单文件，无安装包，拷到别的机器即可双击；依赖系统 VC++ 运行库，
极老的系统若提示缺 `VCRUNTIME140.dll`，装一次微软 VC++ 运行库即可）。

- 生成的是 **无控制台窗口** 的 GUI 程序。
- exe 的图标、文件版本、产品名称由 `build.rs` 通过 `embed-resource` 嵌入：图标源是
  `assets/periphhub.ico`，版本号自动取自 `Cargo.toml`，改版本只需改一处。
- `Cargo.toml` 里 release 配置为 `opt-level = "s"` + `lto = true` + `codegen-units = 1`，
  体积小但**编译慢**（本机实测冷编译约 2 分 50 秒，产物约 11 MB），改代码后频繁调试建议先用 `cargo build`。

正式发布走便携 zip：

```powershell
pwsh scripts/package.ps1              # 构建并打包到 dist\
pwsh scripts/package.ps1 -SkipBuild  # 复用已有的 release 构建
```

`dist\` 下会生成 `PeriphHub-v<版本>-win-x64.zip`（exe、README、LICENSE、本文件）和
`SHA256SUMS.txt`。推送 `v*` tag 后，`.github/workflows/release.yml` 会在 Windows runner 上
运行 `cargo test`、执行同一个打包脚本，并自动创建 GitHub Release 附加 zip 和校验和。
**tag 必须与 `Cargo.toml` 的 `version` 一致**（如 `v0.2.0` ↔ `0.2.0`），否则工作流直接失败。

当前未做代码签名，用户首次运行会看到 SmartScreen“未知发布者”提示；如果以后接入证书，
在打包脚本之后、创建 Release 之前加一步 `signtool sign /fd SHA256 /tr <时间戳服务> ...` 即可。

跑单元测试（只覆盖设备分类、命名、协议常量等纯逻辑，不碰硬件）：

```powershell
cargo test
# 51 passed; 1 ignored（hid::dump::dump_devices 是手动导出枚举结果用的）
```

---

## 5. 启动后会发生什么

1. **无系统边框窗口**：自定义标题栏，可拖动、双击最大化、右下角/边缘拖拽缩放；
   最小尺寸 540×480。
2. **首页设备卡片**：插入的设备会自动出现，每张卡显示连接方式、当前 DPI/档位、回报率、电量。
3. **轮询节奏**：完整配置快照每 **4 秒**读一次，电量每 **20 秒**一次；改动参数后只回读被改的那个区块，
   不做全量刷新。两条规则保证"改一下就被读回来"不会发生：
   - 队列里的命令按字段合批，只写最后那个值（按住 `+` 一秒 = 一次写入，不是十次）；
   - 每条快照都带"读它时已处理到第几条命令"，比界面最后发出的命令更旧的快照直接丢弃，
     所以回读不会把手上的修改顶回去，也不会让下一次 `±` 从旧值起算。
4. **两种外壳，一个窗口**：同一个原生窗口会在"主界面"和"桌面浮窗"之间切换尺寸，
   不是两个窗口，也没有隐藏状态。**首次启动（没有任何配置文件）直接进浮窗**。
   - **主界面** 960×680（最小 540×480），用于配置设备，通常只在需要改参数时打开。
   - **桌面浮窗** 296 宽、高度按设备数自适应，常驻桌面，一眼能看到每台设备的连接状态和电量。
5. **进入浮窗**：点标题栏关闭按钮 / 设置里的"收起为浮窗" / 托盘菜单勾"桌面浮窗"。
   主界面关闭不会退出程序，而是缩成浮窗。
6. **退出**：浮窗右上角的 ×，或托盘菜单"退出"。浮窗模式下 Alt+F4 等同 ×。
7. **浮窗的交互**：
   - 拖顶部（图标与按钮之间）可移动，位置记在配置里；双击顶部 = 打开主界面。
   - 点某台设备 = 打开主界面并直接进入该设备的配置页。
   - 顶部三个按钮：置顶（默认开）/ 打开主界面 / 退出。
   - 有离线或低电量设备时，底部出现"n 台设备需要关注"，点击打开主界面。
   - 浮窗尺寸随设备数变化，超过 5 台后列表内部滚动，不会把桌面占满。
8. **托盘菜单**：打开主界面 / 桌面浮窗（勾选当前模式）/ 深色主题 / 开机自启动 / 重新扫描设备 / 退出。
   左键单击托盘图标等同"打开主界面"。
9. **托盘创建失败**：会自动退回主界面，并在页面顶部红色提示条里给出原因（12 秒后自动消失，
   也可点"忽略"）。
10. **配置持久化**：`%APPDATA%\PeriphHub\config.json`，内容形如：

   ```json
   {
     "theme": "Light",
     "mini_pos": [1600.0, 756.0],
     "mini_pinned": true,
     "main_pos": null,
     "main_size": null
   }
   ```

   `theme` 可取值 `Dark` / `Light`。当前处于主界面还是浮窗是运行时状态，
   **不写进这个文件**：启动固定从浮窗开始，主界面随时能一键切回去。文件缺失或
   解析失败时一律回落默认值（深色 + 桌面浮窗），不会报错。
11. **开机自启动**：开关在"设置 → 开机自启动"，写入
   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的 `PeriphHub` 值（带引号的 exe 绝对路径）。
   程序每次启动时若发现自启动已开，会重写一次该值，这样 exe 改名或挪目录后路径不会失效。

> 注意：浮窗是**透明无边框窗口**，卡片贴满整个窗口，不留透明留白；圆角由 EGUI 手绘（14px，
> 带 1px 抗锯齿），窗口四周不会出现额外的透明带。
>
> **窗口顶部那条 1px 深线的来由（已修）**：winit 对无边框窗口保留 `WS_CAPTION` / `WS_SIZEBOX`
> 样式，靠改写非客户区（`WM_NCCALCSIZE`）把标题栏藏起来；为了让 DWM 仍画一层投影，它会故意把
> 客户区下移 1px、下方多留 1px，于是窗口最顶上露出**一条属于系统的 1px 非客户区**，表现为一条
> 深色横线（主界面和浮窗都有）。winit 自己把这个开关叫 `undecorated_shadow`，注释里也写了
> "enabling the shadow causes a thin 1px line to appear on the top"。
>
> 现在的做法是两步：
>
> 1. 建窗之后调用 `winit::platform::windows::WindowExtWindows::set_undecorated_shadow(false)`
>    （`app::window_shell`），把客户区还原成整窗，顶部那条深线随之消失；代价是没有窗口投影——反正
>    投影是矩形的，压在圆角卡片四周反而更难看。
> 2. Windows 10 仍会给带 `WS_CAPTION` / `WS_BORDER` 的顶层窗口画 1px 描边，颜色跟随系统浅色/深色
>    模式（所以在不同主题下看到不同颜色），且没有可关的 `DWMWA_*` 属性（那两个属性只有 Win11 认）。
>    因此 `app::Panel::strip_window_frame` 每帧摘掉 `WS_CAPTION` / `WS_BORDER` 与 `WS_EX_WINDOWEDGE`；
>    `WS_SIZEBOX` 必须留着，winit 的边缘缩放是靠 `WM_NCLBUTTONDOWN` 交给系统处理的，没有这个样式位
>    就拖不动。主界面和浮窗都会执行，桌面窗口也不会被排除到任务栏/Alt+Tab 之外（那需要 `WS_POPUP`，
>    这里没有设）。
>
> 摘样式后屏幕上剩下的唯一一圈线，是界面自己画的卡片描边（`theme::Palette::border`，浅色 229、深色 46）。
> Windows 11 上额外把 `DWMWA_BORDER_COLOR` 设为 `NONE`、`DWMWA_WINDOW_CORNER_PREFERENCE` 设为
> `DONOTROUND`，避免系统圆角叠在手绘圆角上；Windows 10 不支持这两个属性，调用失败即忽略。

---

## 6. 支持的设备

| 设备 | VID:PID | 能力 |
| --- | --- | --- |
| 雷柏 VT1 Pro（2.4G 接收器） | `24AE:1214` | 完整读写：DPI 各档位、回报率、静默高度、移动同步、按下/抬起防抖、休眠时间、直线/波纹修正、当前档位、电量 |
| 雷柏 VT1 Pro（插线） | `24AE:4414` | 与接收器完全相同的能力；同一个鼠标插线后换成这个 PID，配置读的是鼠标本体 |
| 罗技 G304（Lightspeed 接收器） | `046D:C53F` | 5 档 DPI（200–12000，可改可切，写入板载配置）、回报率 125–1000 Hz、电量、设备名 |
| 狼蛛 F87（含 2.4G 接收器） | `3554:FA09` | 只读：能在列表里看到设备与连接方式，但没有可读写参数 |
| RK98（含 2.4G 接收器） | `25A7:FA70` | 只读：能识别设备，但连电量都读不到 |
| 键盘插线时的控制器 | `258A:010C` | 只读：F87/RK98 用线连接后都以 BY Tech 这个控制器出现，只有 feature 报表、没有应答通道，协议未破解（见 `docs/aula-f87.md` §9） |

插线不是"同一个设备换条线"这么简单：鼠标在有线模式下换成另一个 PID（`0x4414`），
键盘换成 BY Tech 的通用控制器（`258A:010C`）。设备表里用 `DeviceProfile::wired` 区分这两种形态，
`hid::classify_connection` 按表判定连接方式——只看实例路径的话，接收器和插线设备都是 `USB\` 开头，
分不出来。

三套协议分别放在 `src/driver/` 下：`rapoo.rs`（32 字节 `0xBA` 帧，EEPROM 读写）、
`hidpp.rs`（罗技 HID++ 2.0，20 字节 `0x11` 帧）。只读设备没有驱动，只在 `driver::SUPPORTED` 总表里登记（`driver` 字段为 `None`）。

为什么后两台只读：

- **F87**：面板的配置通道要求 HID 集合是 `usage page 0xFF00 / usage 0x0E`（32 字节 `0xBA` 报告），
  而 F87 接收器的厂商通道是 `0xFF02 / 0x02`（20 字节 `0x13` 帧）。协议已摸清但还没接入，
  详见 `docs/aula-f87.md`。
- **RK98**：它的接收器**没有任何输出端点**，厂商集合只有一个 8 字节 feature 记录，其中仅 byte1
  可写，且写完没有任何回值通道；被动监听 90 秒也是 0 帧。这条链路上不存在"能读回结果"的通道，
  详见 `docs/rk98.md`。

能力差异直接反映到界面：`driver::Caps` 决定渲染哪些卡片。G304 有 DPI 档位和回报率两套卡片，
但**没有**传感器参数卡（LOD/防抖/修正这些它固件里根本没有）；只读设备的页面则直接给出具体原因，
而不是笼统的"未找到配置接口"。

罗技这家还多一层：DPI 档位和回报率都存在鼠标的**板载配置扇区**里，写一次要"读扇区 → 改字节 →
重算 CRC16 → 分块写 → 提交"，约 300 ms，所以拖动式连续微调会被合批成最后一次。

界面上的所有开关、滑块、输入框都是**即时写入设备**（雷柏是读 4 字节块 → 改字节 → 写回 →
`0xA3` 提交；罗技是 HID++ `setSensorDpi` 后回读确认），没有"保存/应用"按钮，改完就已经生效。

不常用的选项（移动同步、直线修正、波纹修正、按下/抬起防抖、休眠、静默高度）在开关下面都有一行
说明文字和一个蓝色"推荐 xxx"标签，用来解释这个开关到底改什么、多数场景下填多少合适。
DPI 档位、回报率只显示数值，不带任何进度/量程条——这两项没有"上限"可比，进度条只会误导。

DPI 的每一档都能**直接输入**：点上方档位芯片切换要改的那一档（同时会真的切到该档），
在下方的输入框里填该设备允许的范围（雷柏 100–26000，G304 200–12000），回车或移开焦点即写入；
右侧 `−`/`+` 仍可按住连续微调（每次 50 / 100）。

参数卡右上角的「刷新配置」是一个不带边框的文字按钮（图标 + 文案，悬停才浮出一层浅底）：
它只重新读一遍设备上的当前值，不该比旁边的连接徽标更显眼。

页面最下面的「系统指针」卡不是设备参数：指针速度（Windows 滑块 1–20，默认 10）、滚轮行数、
整屏滚动三项走的是 `SystemParametersInfo`，改的是整台电脑，换机器不会跟着走。写入被组策略或
安全软件拒绝时，卡片下方会弹出"这次修改没有生效"的提示，不会静默失败。

---

## 7. 配套诊断工具（examples）

`examples/` 下有三个独立的命令行探针，编译方式与主程序一致，用于协议排查和新设备适配。
**运行前先关掉面板**——同一设备共用应答 buffer，两边同时轮询会互相读到对方的应答。

### 7.1 probe —— 枚举 HID 集合与描述符

```powershell
# 枚举所有 HID 集合、caps、报表描述符，并对厂商集合做只读探测
cargo run --example probe

# 只看某个 VID（十六进制，可带 0x 前缀）
cargo run --example probe -- 3554
cargo run --example probe -- 24ae

# 额外动作（与 VID 参数可同时使用）
cargo run --example probe -- 3554 setfeat   # 对 0xFF04 feature 集合做写入试探
cargo run --example probe -- 3554 cmds      # 扫描 feature 命令字
```

### 7.2 hidpp —— 罗技 HID++ 2.0 只读探测

```powershell
cargo run --example hidpp            # 枚举接收器上的设备、功能索引、电量、DPI、板载信息
cargo run --example hidpp -- 1       # 只探测设备索引 1
```

只发读取类请求，不会改设备。G304 的功能**只在 20 字节长通道（`0xFF00/0x0002`）上响应**，
7 字节 short 通道一律回 HID++ 1.0 的 `0x8F` 错误帧——这点和其他罗技设备不同，
细节见 `docs/logitech-g304.md`。

### 7.3 f87_battery —— 狼蛛 F87 的 20 字节帧验证

```powershell
cargo run --example f87_battery                       # 默认：发 3 次电量查询（0x4A）
cargo run --example f87_battery -- --watch 120        # 持续查询 120 秒
cargo run --example f87_battery -- --battery          # 电量查询，6 次重试并全量打印回帧
cargo run --example f87_battery -- --config           # 读 128 字节配置块并解析字段
cargo run --example f87_battery -- --test             # 先读配置证明链路活着，再查电量
cargo run --example f87_battery -- --fuzz             # 扫命令族；扫描前会把配置快照写 target\f87_config_before.txt
cargo run --example f87_battery -- --scan 50-5f       # 指定命令区间
cargo run --example f87_battery -- --idle 30          # 静默监听 30 秒，看有无主动上报
cargo run --example f87_battery -- --send "13 44 01 00"  # 发任意帧（自动补校验和）
```

三个工具的完整输出说明见各自文件头部注释。`probe` 会监听所有 `0xFFxx` 且有输入报表的集合，
F87 的 20 字节通道和 RK98 的 8 字节通道都能一并看到。

### 7.4 rapoo_selftest —— 雷柏鼠标读写自检

```powershell
cargo run --example rapoo_selftest
```

按驱动完全相同的帧序列跑一遍：只读快照（回报率 / LOD / 移动同步 / 档位 / DPI 表 /
防抖 / 休眠 / 修正位 / 电量）、每个字段"原值写回 + 0xA3 提交 + 回读"、以及一次
DPI `+100 → 还原` 往返。正常不会留下持久改动；还原失败会明确告警，按提示改回即可。

**跑之前先退出 PeriphHub**：两个程序共享应答缓冲，会互相读到对方的应答。

自检的应答判据与驱动一致：同一地址、同一长度的重复读取允许内容不变，不同地址仍要求
与上一条不同（`docs/protocol.md` §3.2）。写新工具时别只按"内容必须不同"判断新应答，
否则写后回读或相同值连读会被自己判成过期而超时。

---

## 8. 常见问题

| 现象 | 原因 | 处理 |
| --- | --- | --- |
| 首页空白，一台设备都没有 | 接收器没插好 / 设备在蓝牙或关机状态 / 设备不在 `driver::SUPPORTED` 总表里且没有 `0xFF00:0x0E` 通道 | 确认 2.4G 接收器在位、设备已开机；点"重新扫描"；用 `cargo run --example probe` 看系统到底枚举到了什么 |
| 卡片一直"正在连接"，随后变"设备离线" | 设备休眠、被厂商驱动独占、或应答超时 | 动一下鼠标/敲一下键盘唤醒；关掉官方驱动和同类面板；点"重新连接" |
| 改了参数过几秒又弹回原值 | 读到的是上一条应答（设备被吞命令时会重复回旧数据） | 先确认没有别的程序在轮询同一设备；正常情况下面板已做了"应答去重 + 40 ms 稳定期 + 15 ms 命令间隔 + 超时重发"，若仍复现，用 `f87_battery`/`probe` 观察真机应答 |
| 提示"打开设备失败（… os error 32）" | 设备被其它程序以独占方式打开（共享模式冲突） | 退出官方驱动 / 其它外设工具后重新扫描 |
| 提示"设备无响应（超时）" | 命令被吞或链路空闲后首包慢（空闲后首条命令可能要 150–250 ms） | 再操作一次；仍失败就重新扫描。协议层的重发与超时预算见 `docs/protocol.md` 第 3 节 |
| G304 一直"设备离线" | 鼠标休眠后不再应答 HID++（接收器仍然在线，所以设备列表里还在） | 动一下鼠标唤醒；面板每 1.5 秒会重试握手，唤醒后自动恢复 |
| G304 点了档位芯片但指针速度没变 | 鼠标休眠时 HID++ 设置当前 DPI 的命令没有应答 | 动一下鼠标唤醒再操作；面板现在把"无应答"当作失败而不是成功，几秒后的周期读取会把真实值带回来 |
| RK98 / F87 页面没有任何开关 | 这两台设备的接收器没有可写的厂商通道（详见 `docs/rk98.md`） | 属于设备限制，不是缺功能；页面顶部那句说明写的就是具体原因 |
| 双击 exe 只出来一个小浮窗 | 默认行为：启动固定进入浮窗（避免一开机就盖住桌面），当前在哪种壳不落盘 | 点浮窗右上角的"打开主界面"，或在托盘菜单点"打开主界面"；主界面右上角也有"切换为浮窗模式"的按钮 |
| 浮窗不见了 | 取消了置顶且被其它窗口盖住，或位置记住了屏幕外 | 现在启动时会自动校验保存的坐标：整个窗口都落在所有显示器之外（比如拔掉了副屏）就回到默认位置，无需手工删 `mini_pos` |
| 想知道设备为什么离线 / 上次为什么崩 | release 版没有控制台，线程 panic 默认看不见 | 看 `%LOCALAPPDATA%\PeriphHub\log\periphhub.log`（启动、采集线程异常退出都会记录；超过 1 MB 自动滚动成 `.log.1`）。设备读取线程崩溃后面板会自动重建，60 秒内连崩 3 次才转离线提示 |
| 托盘图标不见了 / 浮窗点 × 就退出 | 托盘创建失败，程序已回退主界面 | 看页面顶部红色提示条的原因；托盘依赖 explorer.exe，重启资源管理器后重新扫描 |
| 编译报错找不到 `link.exe` | 缺 MSVC 生成工具 | 安装 Visual Studio Build Tools 的"C++ 桌面开发"工作负载，或 `rustup default stable-x86_64-pc-windows-msvc` 后重开终端 |
| 界面字体是方块 | 系统缺 `simhei.ttf` / `msyh.ttc` | 装上黑体或微软雅黑（`C:\Windows\Fonts`），程序按这两个路径顺序加载中文字体 |

---

## 9. 目录结构与相关文档

```
src/
  main.rs        入口：窗口参数、图标、按配置决定启动成主界面还是浮窗
  app.rs         应用外壳：两种外壳的切换与窗口尺寸、标题栏、设置弹窗、托盘事件分发
  devices.rs     设备管理器：枚举、每设备工作线程、命令队列、快照轮询节奏
  driver/        协议驱动层：向上只暴露 Cmd / Snapshot / Caps
    mod.rs       能力位、命令定义、Driver trait、VID/PID → 协议映射
    rapoo.rs     雷柏 VT1 Pro：32 字节 0xBA 帧 + EEPROM 读写
    hidpp.rs     罗技 HID++ 2.0：电量、DPI、设备名
  hid.rs         HID 枚举：SetupAPI、caps、容器分组、设备表与只读原因
  config.rs      %APPDATA%\PeriphHub\config.json 读写
  autostart.rs   HKCU Run 键的增删查
  tray.rs        托盘图标与菜单
  demo.rs        演示模式用的假设备
  theme.rs       配色、字体、电池配色、程序图标
  ui/            home（设备工作台）/ mini（桌面浮窗）/ config_page（参数面板）/ chrome（标题栏、缩放）/ widgets
examples/
  probe.rs       通用 HID 枚举与只读探测
  hidpp.rs       罗技 HID++ 2.0 功能扫描
  f87_battery.rs 狼蛛 F87 帧验证
docs/
  protocol.md       雷柏 VT1 Pro（24AE:1214）HID 协议笔记
  logitech-g304.md  罗技 G304 / 046D:C53F 的 HID++ 实测笔记
  rk98.md           RK98 / 25A7:FA70：为什么 2.4G 下写不了
  aula-f87.md       狼蛛 F87 / 3554:FA09 实测笔记
  gap-analysis.md   与雷柏官方驱动的功能差距对照（含未实现项的技术评估）
assets/
  periphhub.ico     exe 与窗口图标源文件
build.rs            把图标、版本信息和产品名嵌入 exe
scripts/package.ps1 本地打包脚本（GitHub Actions 复用）
.github/workflows/release.yml  推送 v* tag 时自动发布
README.md           下载、功能、构建与发布说明
LICENSE             MIT
```

卸载/重置：程序不写系统目录、不装服务。删掉 exe 即卸载；想连配置一起清干净，
删除 `%APPDATA%\PeriphHub` 目录，并在注册表里删掉
`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的 `PeriphHub` 值（或先在设置里关掉自启动）。
