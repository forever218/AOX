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
;      dist\AOX-GUI-Setup-<version>.exe                 (main package, bash sidecar only, ~4.6 MB)
;      dist\AOX-GUI-Setup-<version>-with-python.exe     (bundled variant, ~13.1 MB)
;
;  Relationship to aox.iss (the CLI edition): the two are independent installers and share
;  neither the AppId, the install directory nor the Start-menu entry, so both can be installed
;  on one machine without interfering with each other.
;
;  Design notes (and why they match the CLI edition):
;    * Installs for the **current user only** (PrivilegesRequired=lowest), into
;      %LOCALAPPDATA%\Programs\AOX-GUI -- the GUI and the CLI share the root-directory
;      resolution in aox::paths, and at runtime they need to write config.toml and plugins/
;      inside their own directory, which under Program Files would fail for lack of write
;      permission. The folder name has **no space** (2026-10-25, user request): the install
;      directory is where config.toml, plugins\ and history\ live, and every command that
;      touches it -- `cd`, a path in a shell, a shortcut argument -- would otherwise need
;      quoting. The *display* name stays "AOX GUI" (see AppName), which is a label, not a path.
;    * Renamed from `AOX GUI` on 2026-10-25. `UsePreviousAppDir=no` is what makes the rename
;      take effect: without it Setup pre-fills the old path from the uninstall registry entry
;      (same AppId) and an upgrade would quietly stay in the old folder. The consequence is
;      that an install of the old edition is **left untouched**: its config.toml, plugins\ and
;      history\ have to be moved across by hand (or copied before uninstalling), because the
;      new folder starts empty.
;    * config.toml is not packaged (so a personal API key is never baked into the installer);
;      the program generates one on first run.
;    * plugins/ is not packaged either: the host creates plugins/sdk on startup.
;    * assets/light + assets/dark ARE packaged: the first screen reads them at run time and picks
;      one at random per theme (see gui/src/brand.rs). They are removable -- with no folder the
;      exe falls back to the pair embedded in it -- so losing them degrades the look, not the app.
;    * Uninstalling does not delete plugins/ -- capability the model accumulated is kept.
;    * PATH is not written: the GUI edition doesn't need launching from a terminal, and this
;      avoids fighting with the CLI edition's PATH entry.
;    * **Every** variant ships runtime\bash (busybox-w32, ~637 KB): the `bash` tool needs no
;      python, and a machine without Git for Windows would otherwise fall back to `shell` alone.
;      Only the -with-python variant adds runtime\python.
; ============================================================================

#define AppName        "AOX GUI"
; The short name: file names, the **install folder** (`%LOCALAPPDATA%\Programs\AOX-GUI`) and the
; Start-menu group's uninstall key. AppName keeps its space because it is only ever shown, whereas
; this one ends up in paths -- "AOX GUI-Setup-0.1.0.exe" would need escaping in URLs, terminals and
; scripts alike, and an install directory with a space needs quoting on every `cd` and every path
; handed to a command. Same reason the CLI edition is simply `AOX`.
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

; The **slug**, not the display name: this directory holds config.toml / plugins\ / history\, and a
; space in it has to be quoted in every command that names it.
DefaultDirName={localappdata}\Programs\{#AppSlug}
; Off for the 2026-10-25 rename only: with it on, Setup pre-fills the directory remembered by the
; previous install (same AppId) and the new folder would never be used. **Put it back to `yes` in
; the next release** -- the cost of leaving it off is that anyone who installs into a custom
; directory stops having it remembered by later upgrades.
UsePreviousAppDir=no
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
; The first screen's background pools (`gui/src/brand.rs`): at run time the app lists
; `assets\light\*.png` / `assets\dark\*.png` and picks one of each at random, re-rolling on
; startup and on every new conversation. Missing them is not fatal -- the exe falls back to the
; pair embedded in it -- hence `skipifsourcedoesntexist`: a checkout whose folders are still
; empty must not fail the build.
Source: "{#ProjectRoot}\assets\light\*.png"; DestDir: "{app}\assets\light"; Flags: ignoreversion skipifsourcedoesntexist
Source: "{#ProjectRoot}\assets\dark\*.png";  DestDir: "{app}\assets\dark";  Flags: ignoreversion skipifsourcedoesntexist
; ── every variant: the bash sidecar ────────────────────────────────────────────────────────────────
; busybox-w32 (~637 KB, one file, no DLLs) gives the model a POSIX shell on machines that have no Git
; for Windows. It is shipped in **all** variants — the `bash` tool is useful without python, and the
; host picks it last (a real bash, if installed, always wins: see src/shell/bash.rs). Its build id and
; the GPLv2 obligations are recorded in runtime\bash\README.md, which travels with it.
Source: "{#ProjectRoot}\runtime\bash\*"; DestDir: "{app}\runtime\bash"; \
    Flags: ignoreversion recursesubdirs createallsubdirs
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
; It exists in every build now (it holds the bash sidecar) and in the bundled build additionally
; holds python; everything in it is re-provisionable, so removing it on uninstall is right.
Type: filesandordirs; Name: "{app}\runtime"

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\aox.ico"; WorkingDir: "{app}"; Comment: "{#AppName} - graphical front-end for the AOX agent"
Name: "{autodesktop}\{#AppName}";  Filename: "{app}\{#AppExeName}"; IconFilename: "{app}\aox.ico"; WorkingDir: "{app}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "{cm:LaunchApp}"; WorkingDir: "{app}"; Flags: nowait postinstall skipifsilent
