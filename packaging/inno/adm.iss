; Inno Setup script untuk Alpha Download Manager (plan §11.2/§14 WM7).
; Build dulu: cargo build --release ; lalu kompilasi skrip ini dengan ISCC.
; Disarankan tandatangani exe + installer (lihat packaging/README.md).

#define MyAppName "Alpha Download Manager"
#define MyAppVersion "0.1.0"
#define MyAppPublisher "ADM"
#define MyAppExe "adm-app.exe"

[Setup]
AppId={{B6A1F2C0-ADM0-4A10-9E00-AlphaDownloadMgr}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher={#MyAppPublisher}
DefaultDirName={autopf}\ADM
DefaultGroupName=ADM
DisableProgramGroupPage=yes
OutputDir=..\..\target\installer
OutputBaseFilename=adm-setup-{#MyAppVersion}
SetupIconFile=..\..\crates\adm-app\assets\adm.ico
UninstallDisplayIcon={app}\{#MyAppExe}
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=lowest
WizardStyle=modern

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; Flags: unchecked
Name: "autostart"; Description: "Start ADM when Windows starts"; Flags: unchecked

[Files]
Source: "..\..\target\release\adm-app.exe";    DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\adm-bridge.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#MyAppName}";           Filename: "{app}\{#MyAppExe}"
Name: "{group}\Uninstall {#MyAppName}"; Filename: "{uninstallexe}"
Name: "{autodesktop}\{#MyAppName}";     Filename: "{app}\{#MyAppExe}"; Tasks: desktopicon

[Registry]
; Autostart opsional (HKCU\...\Run). Bridge juga bisa men-set ini lewat Options.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
  ValueType: string; ValueName: "ADM"; ValueData: """{app}\{#MyAppExe}"" --tray"; \
  Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\{#MyAppExe}"; Description: "Launch {#MyAppName}"; \
  Flags: nowait postinstall skipifsilent

; Catatan integrasi browser:
; Registrasi native messaging host bergantung pada Extension ID, jadi dijalankan
; setelah extension dipasang:  "{app}\adm-bridge.exe" register <EXTENSION_ID>
; (lihat extension/README.md). Bisa juga ditambah sebagai langkah [Run] manual.
