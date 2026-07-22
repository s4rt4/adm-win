; Installer ADM (Inno Setup 6). Jangan kompilasi langsung — pakai tools\make-installer.ps1
; yang menyiapkan folder staging lalu memanggil ISCC dengan /DStaging=... /DAppVersion=...
#ifndef Staging
  #error "Jalankan lewat tools\make-installer.ps1 (butuh /DStaging=<folder>)"
#endif
#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#define ExtensionId "cjamijdkchdmdocnpdbjobcnmagdjcfe"

[Setup]
AppId={{B7A2F6D1-4C3E-4E8A-9B0D-ADM000000001}
AppName=Alpha Download Manager
AppVersion={#AppVersion}
AppPublisher=s4rt4
DefaultDirName={localappdata}\ADM
DisableDirPage=auto
DisableProgramGroupPage=yes
; Per-user: tanpa UAC; cocok dengan lokasi produksi %LOCALAPPDATA%\ADM.
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputBaseFilename=ADM-Setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
CloseApplications=yes
RestartApplications=no
SetupIconFile={#SourcePath}\..\crates\adm-app\assets\adm.ico
UninstallDisplayIcon={app}\adm-app.exe
UninstallDisplayName=Alpha Download Manager

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#Staging}\adm-app.exe";    DestDir: "{app}"; Flags: ignoreversion
Source: "{#Staging}\adm-bridge.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Staging}\deno.exe";       DestDir: "{app}"; Flags: ignoreversion
Source: "{#Staging}\ffmpeg.exe";     DestDir: "{app}"; Flags: ignoreversion
Source: "{#Staging}\ffprobe.exe";    DestDir: "{app}"; Flags: ignoreversion
Source: "{#Staging}\yt-dlp.exe";     DestDir: "{app}"; Flags: ignoreversion
; Extension MV3 disertakan agar bisa di-load unpacked dari folder instal;
; ID-nya stabil (field "key" di manifest) sehingga cocok dengan register di bawah.
Source: "{#Staging}\extension\*";    DestDir: "{app}\extension"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{autoprograms}\ADM";  Filename: "{app}\adm-app.exe"
Name: "{autodesktop}\ADM";   Filename: "{app}\adm-app.exe"; Tasks: desktopicon

[Run]
; Tulis manifest native messaging + registry HKCU dari lokasi instal.
Filename: "{app}\adm-bridge.exe"; Parameters: "register {#ExtensionId}"; Flags: runhidden
Filename: "{app}\adm-app.exe"; Description: "{cm:LaunchProgram,ADM}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{app}\adm-bridge.exe"; Parameters: "unregister"; Flags: runhidden; RunOnceId: "UnregBridge"

[Code]
// Matikan proses ADM yang sedang jalan agar salin file tidak gagal file-locked.
procedure KillAdmProcesses();
var
  R: Integer;
begin
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM adm-app.exe /T', '',
       SW_HIDE, ewWaitUntilTerminated, R);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM adm-bridge.exe /T', '',
       SW_HIDE, ewWaitUntilTerminated, R);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  KillAdmProcesses();
  Result := '';
end;

function InitializeUninstall(): Boolean;
begin
  KillAdmProcesses();
  Result := True;
end;
