; ============================================================================
;  AOX GUI 安装脚本（Inno Setup）
;
;  编译：
;      "E:\INNO SETUP\Inno Setup 7\ISCC.exe" installer\aox-gui.iss
;  产物：
;      dist\AOX-GUI-Setup-<版本>.exe
;
;  与 aox.iss（命令行版）的关系：两者是各自独立的安装包，AppId、安装目录、
;  开始菜单项都不共用，可以同时装在一台机器上互不干扰。
;
;  设计要点（与命令行版保持一致的理由）：
;    * 采用「当前用户」安装（PrivilegesRequired=lowest），装到
;      %LOCALAPPDATA%\Programs\AOX GUI —— GUI 与 CLI 共用 aox::paths 的
;      根目录解析，运行时要在自己的目录下写 config.toml 和 plugins/，
;      装在 Program Files 会因无写权限而失效。
;    * 不打包 config.toml（避免把个人 API key 装进去），由程序首次运行自动生成。
;    * 不打包 plugins/：宿主启动时会自动创建 plugins/sdk。
;    * 卸载不删除 plugins/，保留模型积累的能力。
;    * 不写 PATH：GUI 版不需要从终端启动，避免和命令行版的 PATH 项打架。
; ============================================================================

#define AppName        "AOX GUI"
; 文件名用的短名：AppName 里有空格，直接拿来当文件名会得到
; 「AOX GUI-Setup-0.1.0.exe」，在 URL、终端和脚本里都要额外转义，所以分开定义。
#define AppSlug        "AOX-GUI"

; 版本号可由命令行覆盖，避免与 Cargo.toml 脱节：
;     ISCC /DAppVersion=1.2.3 installer\aox-gui.iss
; tools/build_installer.sh 会自动从 Cargo.toml 读取并传入。
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

#define AppPublisher   "lisiran"
#define AppURL         "https://2am.top"
#define ProjectURL     "https://github.com/forever218/AOX"
#define AppExeName     "aox-gui.exe"
#define ProjectRoot    ".."

; ── 可选变体：把 Python 运行时装进安装包 ──────────────────────────────────
;      ISCC /DAppVersion=1.2.3 /DBUNDLE_PYTHON=1 installer\aox-gui.iss
;   产出 AOX-GUI-Setup-<版本>-with-python.exe，装完即可用 python 插件（离线可用）。
;   与命令行版完全同构：主包不捆绑，需要时才装配或用 -with-python 变体。
#ifdef BUNDLE_PYTHON
  #define OutputSuffix "-with-python"
#else
  #define OutputSuffix ""
#endif

[Setup]
; 与命令行版的 AppId 必须不同，否则两个包会互相覆盖
AppId={{7D4E1B92-3A5C-4F8E-9B26-5C1A7E3D82F4}
AppName={#AppName}
AppVersion={#AppVersion}
AppVerName={#AppName} {#AppVersion}
AppPublisher={#AppPublisher}
AppPublisherURL={#ProjectURL}
AppSupportURL={#ProjectURL}
AppUpdatesURL={#ProjectURL}
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

OutputDir={#ProjectRoot}\dist
OutputBaseFilename={#AppSlug}-Setup-{#AppVersion}{#OutputSuffix}
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
SetupMutex=AOX_GUI_Setup_Mutex

[Languages]
Name: "chinese"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
chinese.LaunchApp=立即启动 {#AppName}
english.LaunchApp=Launch {#AppName}

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#ProjectRoot}\target\release\{#AppExeName}"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#ProjectRoot}\assets\aox.ico";               DestDir: "{app}"; Flags: ignoreversion
Source: "{#ProjectRoot}\config.example.toml";          DestDir: "{app}"; Flags: ignoreversion
Source: "{#ProjectRoot}\README.md";                    DestDir: "{app}"; Flags: ignoreversion isreadme
Source: "{#ProjectRoot}\LICENSE";                      DestDir: "{app}"; Flags: ignoreversion
#ifdef BUNDLE_PYTHON
; 捆绑变体：Python 运行时直接装进应用目录，宿主启动时自动发现
; <应用根>\runtime\python\python.exe（三档解析的第 2 档）。
; 运行时由 tools/build_installer.sh 的 ensure_runtime 下载并解压到 runtime\python\。
Source: "{#ProjectRoot}\runtime\python\*"; DestDir: "{app}\runtime\python"; \
    Flags: ignoreversion recursesubdirs createallsubdirs
#endif

[UninstallDelete]
; 运行时是「机器相关、可随时重新装配」的数据（约 21 MB），卸载时清掉；
; 而 plugins\ 是模型积累的能力，必须保留。该目录只在捆绑版或用户主动装配过时才存在。
Type: filesandordirs; Name: "{app}\runtime"

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\aox.ico"; WorkingDir: "{app}"; Comment: "{#AppName} - graphical front-end for the AOX agent"
Name: "{autodesktop}\{#AppName}";  Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\aox.ico"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchApp}"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent
