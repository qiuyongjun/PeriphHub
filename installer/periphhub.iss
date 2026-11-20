; PeriphHub Windows 安装包脚本（Inno Setup 6.5+）
; 版本号由调用方传入：ISCC /DAppVersion=x.y.z（scripts\package.ps1 会自动传），
; 编译前请先执行 cargo build --release。
; 编译：ISCC.exe /DAppVersion=0.2.2 installer\periphhub.iss
;      →  dist\PeriphHub-v<版本>-win-x64-setup.exe

#define MyAppName "PeriphHub"
#define MyAppExeName "PeriphHub.exe"

#ifndef AppVersion
  #error 请通过 ISCC /DAppVersion=x.y.z 编译，或直接使用 scripts\package.ps1
#endif
#define MyAppVersion AppVersion

[Setup]
; AppId 为本次安装的稳定标识，卸载/升级都靠它匹配，不要改动
AppId={{6260BC9C-B0BC-4DED-8CBC-DBE6CD542762}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppVerName={#MyAppName} v{#MyAppVersion}
AppPublisher=qiuyongjun
AppPublisherURL=https://github.com/qiuyongjun/PeriphHub
AppSupportURL=https://github.com/qiuyongjun/PeriphHub/issues
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
OutputDir=..\dist
OutputBaseFilename=PeriphHub-v{#MyAppVersion}-win-x64-setup
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\{#MyAppExeName}
; 应用持有同名单实例互斥锁（src\main.rs）：升级/卸载时安装器据此发现正在
; 运行的实例并提示关闭，避免 exe 被占用导致覆盖失败。
AppMutex=PeriphHub-SingleInstance
; 允许用户选择“仅为当前用户安装”（免管理员权限）
PrivilegesRequiredOverridesAllowed=dialog commandline

[Languages]
Name: "chinesesimplified"; MessagesFile: "ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; \
    GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "..\target\release\{#MyAppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\docs\RUN.md"; DestDir: "{app}\docs"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{group}\{cm:UninstallProgram,{#MyAppName}}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; \
    Tasks: desktopicon

[Run]
; runasoriginaluser：管理员安装时不以提权身份启动应用，否则浮窗会继承
; 管理员权限，与普通资源管理器之间的拖拽/交互会受 UIPI 限制。
Filename: "{app}\{#MyAppExeName}"; Description: "{cm:LaunchProgram,{#MyAppName}}"; \
    Flags: nowait postinstall skipifsilent runasoriginaluser

[Code]
// 应用自己在 HKCU Run 里写自启动（src\autostart.rs），卸载器不接管它；
// 这里只在值确实指向本安装目录时删掉，便携版在别处开启的自启动不受影响。
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  RunValue: string;
begin
  if CurUninstallStep = usUninstall then
  begin
    if RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run',
      'PeriphHub', RunValue) then
    begin
      if Pos(Lowercase(ExpandConstant('{app}')), Lowercase(RunValue)) > 0 then
        RegDeleteValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Run',
          'PeriphHub');
    end;
  end;
end;
