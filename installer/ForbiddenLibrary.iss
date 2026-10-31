; Forbidden Library installer. Built by scripts/package.ps1, which stages the
; files in dist\payload and passes /DAppVersion=<version>.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif

[Setup]
AppId={{6C1F5B0E-3A52-4D7B-9E64-2F0B8A1D47C3}
AppName=Forbidden Library
AppVersion={#AppVersion}
AppPublisher=barusoup
AppPublisherURL=https://github.com/barusoup/Forbidden-Library
DefaultDirName={autopf}\Forbidden Library
DefaultGroupName=Forbidden Library
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputBaseFilename=ForbiddenLibrarySetup-{#AppVersion}
SetupIconFile=..\assets\icon\app-icon.ico
UninstallDisplayIcon={app}\forbidden-library.exe
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
Compression=lzma2/normal
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; Flags: unchecked

[Files]
Source: "..\dist\payload\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion

[Icons]
Name: "{autoprograms}\Forbidden Library"; Filename: "{app}\forbidden-library.exe"
Name: "{autodesktop}\Forbidden Library"; Filename: "{app}\forbidden-library.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\forbidden-library.exe"; Description: "{cm:LaunchProgram,Forbidden Library}"; Flags: nowait postinstall skipifsilent

[UninstallDelete]
; Updates add and park files the installer never saw.
Type: filesandordirs; Name: "{app}"
