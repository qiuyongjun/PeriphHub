# PeriphHub

Windows 桌面外设配置面板。不安装厂商驱动、不运行后台服务，直接通过 USB HID 厂商协议读写鼠标/键盘参数。

## 下载

从 [Releases](https://github.com/qiuyongjun/PeriphHub/releases) 下载最新的
`PeriphHub-v*-win-x64.zip`，解压后双击 `PeriphHub.exe` 即可。免安装，不需要管理员权限。

> 程序目前未做代码签名，首次运行时 Windows SmartScreen 可能提示“未知发布者”。
> 点“更多信息”→“仍要运行”即可。Release 附带的 `SHA256SUMS.txt` 可用于校验下载文件。

## 功能

- **桌面浮窗**：常驻桌面，一眼看到每台设备的连接方式、DPI、回报率与电量。
- **设备工作台**：DPI 各档位、回报率、静默高度、移动同步、防抖、休眠、直线/波纹修正等参数直接读写设备。
- **即时生效**：没有“保存/应用”按钮，改完即写入；连续微调会自动合批，避免频繁写 EEPROM。
- **系统指针**：指针速度、滚轮行数、整屏滚动。
- **托盘与自启动**：托盘菜单切换主界面/浮窗、深浅主题、开机自启动、重新扫描；浮窗模式只在托盘显示图标，主界面才出现在任务栏。
- **单文件、无驱动、无服务**：删除 exe 即卸载。

## 支持的设备

| 设备 | VID:PID | 能力 |
| --- | --- | --- |
| 雷柏 VT1 Pro | `24AE:1214` | 完整读写：DPI 档位、回报率、静默高度、移动同步、防抖、休眠、直线/波纹修正、电量 |
| 罗技 G304（Lightspeed 接收器） | `046D:C53F` | 5 档 DPI、回报率、电量、设备名 |
| 狼蛛 F87（含 2.4G 接收器） | `3554:FA09` | 只读：识别设备与连接方式 |
| RK98（含 2.4G 接收器） | `25A7:FA70` | 只读：识别设备 |

只读设备的原因和协议细节见 [docs](docs/) 下的对应笔记。

设备差异集中在 `src/driver/mod.rs` 的 `PROFILES` 表：新增同协议型号优先补 profile（VID/PID、协议、布局、取值范围），而不是复制一份驱动。

## 系统要求

- Windows 10 / 11（x64）
- 显卡驱动支持 OpenGL 3.x
- 极老的系统若提示缺少 `VCRUNTIME140.dll`，安装一次微软 VC++ 运行库即可

## 使用说明

完整的启动行为、浮窗交互、托盘菜单、演示模式和常见问题见 [docs/RUN.md](docs/RUN.md)。

- 配置文件：`%APPDATA%\PeriphHub\config.json`
- 运行日志：`%LOCALAPPDATA%\PeriphHub\log\periphhub.log`
- 手边没有设备时，可设置环境变量 `PERIPHHUB_DEMO=1` 启动演示数据

## 卸载

1. 在设置里关闭“开机自启动”（或删除注册表 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的 `PeriphHub` 值）。
2. 退出程序，删除 `PeriphHub.exe`。
3. 如需清理配置和日志，删除 `%APPDATA%\PeriphHub` 与 `%LOCALAPPDATA%\PeriphHub`。

## 从源码构建

需要 Rust stable（MSVC 工具链）和 Visual Studio 生成工具中的“使用 C++ 的桌面开发”：

```powershell
cargo run                    # 开发运行
cargo test                   # 纯逻辑单元测试，不碰硬件
cargo build --release        # 产物：target\release\PeriphHub.exe
```

## 发布

版本号只在 `Cargo.toml` 中维护。发布新版本：

```powershell
# 1. 修改 Cargo.toml 里的 version，提交改动
# 2. 打 tag 并推送
git tag v0.2.0
git push origin v0.2.0
```

推送 `v*` tag 后，GitHub Actions 会运行测试、构建 release、生成带校验和的 zip，并创建 GitHub Release。
本地也可以手动打包：

```powershell
pwsh scripts/package.ps1              # 产物在 dist\
pwsh scripts/package.ps1 -SkipBuild  # 复用已有 release 构建
```

> tag 必须与 `Cargo.toml` 的版本一致（`v0.2.0` ↔ `version = "0.2.0"`），否则工作流会失败。

## 许可证

[MIT](LICENSE)

PeriphHub 是非官方工具，与雷柏、罗技、狼蛛、RK 等厂商无关联。修改设备参数前请确认风险；
因使用本工具造成的设备异常或数据丢失，作者不承担责任。
