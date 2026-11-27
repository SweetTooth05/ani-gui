; Inno Setup 6 script for ani-gui (Windows x64, per-user, no admin required).
;
; Invoked by packaging/windows/build-windows.sh:
;   iscc /DAppVersion=X.Y.Z /DStageDir=<path> /O<outdir> packaging/windows/ani-gui.iss

#ifndef AppVersion
  #error AppVersion must be defined (pass /DAppVersion=X.Y.Z)
#endif
#ifndef StageDir
  #error StageDir must be defined (pass /DStageDir=<staged prefix directory>)
#endif

[Setup]
; Fixed GUID identifying ani-gui across upgrades (double opening brace escapes '{').
AppId={{E8BAFDF5-7346-41ED-A5C7-9CB380C2FD54}
AppName=ani-gui
AppVersion={#AppVersion}
AppVerName=ani-gui {#AppVersion}
VersionInfoVersion={#AppVersion}
AppPublisher=SweetTooth05
AppPublisherURL=https://github.com/SweetTooth05/ani-gui
AppSupportURL=https://github.com/SweetTooth05/ani-gui/issues
AppUpdatesURL=https://github.com/SweetTooth05/ani-gui/releases
DefaultDirName={localappdata}\Programs\ani-gui
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
CloseApplications=yes
RestartApplications=yes
OutputBaseFilename=ani-gui-windows-x86_64-setup
SetupIconFile=..\..\assets\icons\ani-gui.ico
Uninstallable=yes
UninstallDisplayName=ani-gui
UninstallDisplayIcon={app}\share\icons\ani-gui.ico
Compression=lzma2
SolidCompression=yes
WizardStyle=modern

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[Files]
Source: "{#StageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\ani-gui"; Filename: "{app}\bin\ani-gui.exe"; IconFilename: "{app}\share\icons\ani-gui.ico"; Comment: "Desktop app for watching anime with ani-cli"
Name: "{autodesktop}\ani-gui"; Filename: "{app}\bin\ani-gui.exe"; IconFilename: "{app}\share\icons\ani-gui.ico"; Comment: "Desktop app for watching anime with ani-cli"; Tasks: desktopicon

[Run]
Filename: "{app}\bin\ani-gui.exe"; Description: "{cm:LaunchProgram,ani-gui}"; Flags: nowait postinstall skipifsilent
