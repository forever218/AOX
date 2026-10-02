; ============================================================================
;  AOX 安装脚本（Inno Setup）
;
;  编译（推荐用构建脚本，它会自动编译两遍产出两种变体）：
;      bash tools/build_installer.sh
;  手动编译单个变体：
;      "E:\INNO SETUP\Inno Setup 7\ISCC.exe" installer\aox.iss                          -> 主包
;      "E:\INNO SETUP\Inno Setup 7\ISCC.exe" /DBUNDLE_PYTHON=1 installer\aox.iss         -> 捆绑变体
;  产物：
;      dist\AOX-Setup-<版本>.exe                 （主包，不捆绑运行时，约 3 MB）
;      dist\AOX-Setup-<版本>-with-python.exe     （捆绑变体，约 11.5 MB）
;
;  设计要点：
;    * 采用「当前用户」安装（PrivilegesRequired=lowest），装到
;      %LOCALAPPDATA%\Programs\AOX —— 因为 AOX 运行时要在自己的目录下
;      写 config.toml 和 plugins/，装在 Program Files 会因无写权限而失效。
;    * 不打包 config.toml（避免把个人 API key 装进去），由程序首次运行自动生成。
;    * 不打包 plugins/：宿主启动时会自动创建 plugins/sdk。
;    * 卸载不删除 plugins/，保留模型积累的能力。
;    * 主包不带任何运行时；需要 python 能力时，用户可以 /runtime install python
;      按需装配（离线包或下载），或用上面那个 -with-python 变体一次装好。
; ============================================================================

#define AppName        "AOX"

; 版本号可由命令行覆盖，避免与 Cargo.toml 脱节：
;     ISCC /DAppVersion=1.2.3 installer\aox.iss
; tools/build_installer.sh 会自动从 Cargo.toml 读取并传入。
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

#define AppPublisher   "lisiran"
#define AppURL         "https://2am.top"
#define AppExeName     "aox.exe"
#define ProjectRoot    ".."

; ── 可选变体：把 Python 运行时装进安装包 ──────────────────────────────────
;      ISCC /DAppVersion=1.2.3 /DBUNDLE_PYTHON=1 installer\aox.iss
;   产出 AOX-Setup-<版本>-with-python.exe，装完即可用 python 插件（离线可用）。
;   体积：不捆绑约 3 MB；捆绑后约 13.5 MB（运行时解压后约 21 MB）。
;
;   两种变体共用这一个脚本，靠条件编译区分——不维护第二份 .iss，
;   否则「改了一处忘了另一处」是迟早的事。
#ifdef BUNDLE_PYTHON
  #define OutputSuffix "-with-python"
#else
  #define OutputSuffix ""
#endif

[Setup]
AppId={{9F2C4A17-6B3E-4D5A-8C71-2E9B0D4F1A63}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#AppURL}
AppSupportURL={#AppURL}
AppUpdatesURL={#AppURL}
VersionInfoVersion={#AppVersion}
VersionInfoCompany={#AppPublisher}
VersionInfoDescription={#AppName} Setup
VersionInfoProductName={#AppName}
VersionInfoProductVersion={#AppVersion}

DefaultDirName={localappdata}\Programs\{#AppName}
DefaultGroupName={#AppName}
DisableProgramGroupPage=yes
DisableDirPage=no
AllowNoIcons=yes

; 仅当前用户安装：保证安装目录可写，程序能正常生成 config.toml 与插件
PrivilegesRequired=lowest
ChangesEnvironment=yes

OutputDir={#ProjectRoot}\dist
OutputBaseFilename={#AppName}-Setup-{#AppVersion}{#OutputSuffix}
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
SetupIconFile={#ProjectRoot}\assets\aox.ico
UninstallDisplayIcon={app}\{#AppExeName}
UninstallDisplayName={#AppName} {#AppVersion}
LicenseFile={#ProjectRoot}\LICENSE

ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0.17763
; 防止同一用户重复安装
SetupMutex=AOX_Setup_Mutex

[Languages]
Name: "chinese"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
chinese.AddToPath=将 AOX 添加到 PATH（可在任意终端直接运行 aox）
chinese.LaunchApp=立即启动 {#AppName}
english.AddToPath=Add AOX to PATH (run aox from any terminal)
english.LaunchApp=Launch {#AppName}

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked
Name: "addtopath";   Description: "{cm:AddToPath}";      GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#ProjectRoot}\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#ProjectRoot}\assets\aox.ico";               DestDir: "{app}"; Flags: ignoreversion
Source: "{#ProjectRoot}\config.example.toml";          DestDir: "{app}"; Flags: ignoreversion
Source: "{#ProjectRoot}\README.md";                    DestDir: "{app}"; Flags: ignoreversion isreadme
Source: "{#ProjectRoot}\LICENSE";                      DestDir: "{app}"; Flags: ignoreversion
#ifdef BUNDLE_PYTHON
; 捆绑变体：Python 运行时直接装进应用目录。宿主启动时会自动发现
; <应用根>\runtime\python\python.exe（三档解析的第 2 档），无需改任何配置，
; 也不会往系统 PATH 或用户环境变量里写东西。
; 运行时由 tools/build_installer.sh 的 ensure_runtime 下载并解压到 runtime\python\。
Source: "{#ProjectRoot}\runtime\python\*"; DestDir: "{app}\runtime\python"; \
    Flags: ignoreversion recursesubdirs createallsubdirs
#endif

[UninstallDelete]
; 运行时是「机器相关、可随时重新装配」的数据（约 21 MB），卸载时清掉；
; 而 plugins\ 是模型积累的能力，必须保留（见文件头的设计要点）。
; 该目录只在「捆绑版」或「用户自己 /runtime install 过」时才存在。
Type: filesandordirs; Name: "{app}\runtime"

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\aox.ico"; WorkingDir: "{app}"; Comment: "{#AppName} - self-growing AI agent"
Name: "{autodesktop}\{#AppName}";  Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\aox.ico"; WorkingDir: "{app}"; Tasks: desktopicon

[Registry]
; 可选：把安装目录加入当前用户 PATH（不重复添加）
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
    ValueData: "{olddata};{app}"; Tasks: addtopath; Check: NeedsAddPath('{app}')

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchApp}"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent

[Code]
// 只有当 PATH 中尚无该目录时才写入，避免反复安装把 PATH 撑长
function NeedsAddPath(Param: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, 'Environment', 'Path', OrigPath) then
  begin
    Result := True;
    Exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;
