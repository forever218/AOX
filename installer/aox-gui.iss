; ============================================================================
;  AOX GUI installer script (Inno Setup)
;
;  Build (the build script is recommended: it compiles twice and produces both
;  variants):
;      bash tools/build_installer.sh
;  Build a single variant by hand:
;      "E:\INNO SETUP\Inno Setup 7\ISCC.exe" installer\aox-gui.iss                            -> main package
;      "E:\INNO SETUP\Inno Setup 7\ISCC.exe" --define=BUNDLE_PYTHON=1 installer\aox-gui.iss   -> bundled variant
;
;  Always write definitions in the long form, --define=. The old-style /Dname=value gets
;  rewritten by MSYS as a Unix path under Git Bash (/DAppVersion=0.1.0 becomes
;  <Git install dir>/DAppVersion=0.1.0), and ISCC then counts it as a second script
;  filename and reports "You may not specify more than one script filename."
;  Outputs:
;      dist\AOX-GUI-Setup-<version>.exe                 (main package, ~4 MB)
;      dist\AOX-GUI-Setup-<version>-with-python.exe     (bundled variant, ~12.5 MB)
;
;  Relationship to aox.iss (the CLI edition): the two are independent installers and share
;  neither the AppId, the install directory nor the Start-menu entry, so both can be installed
;  on one machine without interfering with each other.
;
;  Design notes (and why they match the CLI edition):
;    * Installs for the **current user only** (PrivilegesRequired=lowest), into
;      %LOCALAPPDATA%\Programs\AOX GUI -- the GUI and the CLI share the root-directory
;      resolution in aox::paths, and at runtime they need to write config.toml and plugins/
;      inside their own directory, which under Program Files would fail for lack of write
;      permission.
;    * config.toml is not packaged (so a personal API key is never baked into the installer);
;      the program generates one on first run.
;    * plugins/ is not packaged either: the host creates plugins/sdk on startup.
;    * Uninstalling does not delete plugins/ -- capability the model accumulated is kept.
;    * PATH is not written: the GUI edition doesn't need launching from a terminal, and this
;      avoids fighting with the CLI edition's PATH entry.
; ============================================================================

#define AppName        "AOX GUI"
; The short name used in file names: AppName has a space in it, and using it directly as a file
; name gives you "AOX GUI-Setup-0.1.0.exe", which needs escaping in URLs, terminals and
; scripts alike -- hence the separate definition.
#define AppSlug        "AOX-GUI"

; The version can be overridden from the command line, so it can't drift from Cargo.toml:
;     ISCC --define=AppVersion=1.2.3 installer\aox-gui.iss
; tools/build_installer.sh reads it from Cargo.toml and passes it in automatically.
#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

#define AppPublisher   "lisiran"
#define AppURL         "https://2am.top"
#define ProjectURL     "https://github.com/forever218/AOX"
#define AppExeName     "aox-gui.exe"
#define ProjectRoot    ".."

; ── optional variant: bundle the Python runtime into the installer ──────────────────────────────────
;     ISCC --define=AppVersion=1.2.3 --define=BUNDLE_PYTHON=1 installer\aox-gui.iss
; Produces AOX-GUI-Setup-<version>-with-python.exe; python plugins work straight after install
; (usable offline).
; Exactly the same structure as the CLI edition: the main package bundles nothing, and you
; provision on demand or use the -with-python variant.
#ifdef BUNDLE_PYTHON
  #define OutputSuffix "-with-python"
#else
  #define OutputSuffix ""
#endif

[Setup]
; The AppId must differ from the CLI edition's, or the two packages overwrite each other
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

; Current-user install only: guarantees the install directory is writable, so the program can
; generate config.toml and plugins
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
; Stop the same user from installing twice
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
; Bundled variant: the Python runtime goes straight into the application directory, and the
; host discovers <app root>\runtime\python\python.exe on startup (tier 2 of the three-tier
; resolution).
; The runtime is downloaded and unpacked into runtime\python\ by ensure_runtime in
; tools/build_installer.sh.
Source: "{#ProjectRoot}\runtime\python\*"; DestDir: "{app}\runtime\python"; \
    Flags: ignoreversion recursesubdirs createallsubdirs
#endif

[UninstallDelete]
; The runtime is "machine-specific, re-provisionable at any time" data (~21 MB), so it is
; removed on uninstall; whereas plugins\ is capability the model accumulated and history\ is
; the user's conversation record -- both are user data and must be kept.
; Neither directory is listed here, so Inno won't touch them.
; That directory only exists in the bundled build, or once the user has provisioned it
; themselves.
Type: filesandordirs; Name: "{app}\runtime"

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\aox.ico"; WorkingDir: "{app}"; Comment: "{#AppName} - graphical front-end for the AOX agent"
Name: "{autodesktop}\{#AppName}";  Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\aox.ico"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchApp}"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent
