; ============================================================================
;  AOX installer script (Inno Setup)
;
;  Build (the build script is recommended: it compiles twice and produces both
;  variants):
;      bash tools/build_installer.sh
;  Build a single variant by hand:
;      "E:\INNO SETUP\Inno Setup 7\ISCC.exe" installer\aox.iss                                -> main package
;      "E:\INNO SETUP\Inno Setup 7\ISCC.exe" --define=BUNDLE_PYTHON=1 installer\aox.iss       -> bundled variant
;
;  Always write definitions in the long form, --define=. The old-style /Dname=value gets
;  rewritten by MSYS as a Unix path under Git Bash (/DAppVersion=0.1.0 becomes
;  <Git install dir>/DAppVersion=0.1.0), and ISCC then counts it as a second script
;  filename and reports "You may not specify more than one script filename."
;  Outputs:
;      dist\AOX-Setup-<version>.exe                 (main package, bash sidecar only, ~3.6 MB)
;      dist\AOX-Setup-<version>-with-python.exe     (bundled variant, ~12.2 MB)
;
;  Design notes:
;    * Installs for the **current user only** (PrivilegesRequired=lowest), into
;      %LOCALAPPDATA%\Programs\AOX -- because AOX needs to write config.toml and plugins/
;      inside its own directory at runtime, and under Program Files it would have no write
;      permission and break.
;    * config.toml is not packaged (so a personal API key is never baked into the installer);
;      the program generates one on first run.
;    * plugins/ is not packaged either: the host creates plugins/sdk on startup.
;    * Uninstalling does not delete plugins/ or history/: the former is capability the model
;      has accumulated, the latter is the user's conversation record.
;    * history/ isn't pre-created either: the directory is made by the program on the first
;      real conversation, so a session that only runs /help leaves no empty file behind.
;    * **Every** variant ships runtime\\bash (busybox-w32, ~637 KB): the `bash` tool needs no
;      python, and a machine without Git for Windows would otherwise fall back to `shell` alone.
;    * No variant ships the Python runtime except -with-python; when python is needed the user can
;      run /runtime install python to provision it on demand (offline archive or download), or use
;      the -with-python variant above and get everything in one go.
; ============================================================================

#define AppName        "AOX"

; The version can be overridden from the command line, so it can't drift from Cargo.toml:
;     ISCC --define=AppVersion=1.2.3 installer\aox.iss
; tools/build_installer.sh reads it from Cargo.toml and passes it in automatically.
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

#define AppPublisher   "lisiran"
#define AppURL         "https://2am.top"
#define AppExeName     "aox.exe"
#define ProjectRoot    ".."

; ── optional variant: bundle the Python runtime into the installer ──────────────────────────────────
;     ISCC --define=AppVersion=1.2.3 --define=BUNDLE_PYTHON=1 installer\aox.iss
; Produces AOX-Setup-<version>-with-python.exe; python plugins work straight after install
; (usable offline).
; Size: ~3 MB unbundled; ~13.5 MB bundled (the runtime is ~21 MB once unpacked).
;
; Both variants share this one script and are told apart by conditional compilation -- no
; second .iss is maintained, because "changed it here, forgot it there" is only a matter of
; time.
#ifdef BUNDLE_PYTHON
  #define OutputSuffix "-with-python"
#else
  #define OutputSuffix ""
#endif

; ── optional: the language tag in the file name ─────────────────────────────────────────────────────
; The interface language is baked into the exe at build time (`AOX_LANG`), not chosen at install
; time, so two language builds of the same version would otherwise write the exact same file name
; and the second one would silently overwrite the first.
;     ISCC --define=LangTag=-zh installer\aox.iss      ->  AOX-Setup-<version>-zh.exe
; Only the file name carries the tag: display names, AppId and the install folder stay shared, so
; the English and Chinese editions are "the same app in another language" (installing one replaces
; the other) rather than two programs that could both sit in the Start menu.
#ifndef LangTag
  #define LangTag ""
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

; Current-user install only: guarantees the install directory is writable, so the program can
; generate config.toml and plugins
PrivilegesRequired=lowest
ChangesEnvironment=yes

OutputDir={#ProjectRoot}\dist
OutputBaseFilename={#AppName}-Setup-{#AppVersion}{#OutputSuffix}{#LangTag}
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
; Stop the same user from installing twice
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
; ── every variant: the bash sidecar ────────────────────────────────────────────────────────────────
; busybox-w32 (~637 KB, one file, no DLLs) gives the model a POSIX shell on machines that have no Git
; for Windows. It is shipped in **all** variants — the `bash` tool is useful without python, and the
; host picks it last (a real bash, if installed, always wins: see src/shell/bash.rs). Its build id and
; the GPLv2 obligations are recorded in runtime\bash\README.md, which travels with it.
Source: "{#ProjectRoot}\runtime\bash\*"; DestDir: "{app}\runtime\bash"; \
    Flags: ignoreversion recursesubdirs createallsubdirs
#ifdef BUNDLE_PYTHON
; Bundled variant: the Python runtime goes straight into the application directory. The host
; discovers <app root>\runtime\python\python.exe on startup (tier 2 of the three-tier
; resolution), with no configuration change and nothing written into the system PATH or the
; user's environment.
; The runtime is downloaded and unpacked into runtime\python\ by ensure_runtime in
; tools/build_installer.sh.
Source: "{#ProjectRoot}\runtime\python\*"; DestDir: "{app}\runtime\python"; \
    Flags: ignoreversion recursesubdirs createallsubdirs
#endif

[UninstallDelete]
; The runtime is "machine-specific, re-provisionable at any time" data (~21 MB), so it is
; removed on uninstall; whereas plugins\ is capability the model accumulated and history\ is
; the user's conversation record -- both are user data and must be kept (see the design notes
; at the top of this file). Neither directory is listed here, so Inno won't touch them.
; It exists in every build now (it holds the bash sidecar) and in the bundled build additionally
; holds python; everything in it is re-provisionable, so removing it on uninstall is right.
Type: filesandordirs; Name: "{app}\runtime"

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\aox.ico"; WorkingDir: "{app}"; Comment: "{#AppName} - self-growing AI agent"
Name: "{autodesktop}\{#AppName}";  Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\aox.ico"; WorkingDir: "{app}"; Tasks: desktopicon

[Registry]
; Optional: add the install directory to the current user's PATH (without duplicating it)
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
    ValueData: "{olddata};{app}"; Tasks: addtopath; Check: NeedsAddPath('{app}')

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchApp}"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent

[Code]
// Only write when the directory isn't already in PATH, so repeated installs don't keep
// stretching PATH
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
