#ifndef AppVersion
  #define AppVersion "0.2.1"
#endif
#ifndef ArchMode
  #define ArchMode "x64compatible"
#endif
#ifndef OutputBaseFilename
  #define OutputBaseFilename "liftgate-desktop-windows-x86_64-setup"
#endif
#ifndef BuildDir
  #define BuildDir "..\..\dist\win-stage"
#endif
#ifndef OutDir
  #define OutDir "..\..\dist"
#endif
#ifndef IconFile
  #define IconFile "..\icons\app_icon.ico"
#endif

#define MyAppName "Liftgate"
#define MyAppFullName "Liftgate Desktop"
#define MyAppPublisher "acking-you"
#define MyAppURL "https://github.com/acking-you/liftgate"
#define MyAppExeName "liftgate-desktop.exe"

[Setup]
AppId={{8E9D4F32-6C15-4D87-B52B-7B0D5A91C420}
AppName={#MyAppFullName}
AppVersion={#AppVersion}
AppVerName={#MyAppFullName} {#AppVersion}
AppPublisher={#MyAppPublisher}
AppPublisherURL={#MyAppURL}
AppSupportURL={#MyAppURL}/issues
AppUpdatesURL={#MyAppURL}/releases
DefaultDirName={autopf}\{#MyAppName}
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed={#ArchMode}
ArchitecturesInstallIn64BitMode={#ArchMode}
UninstallDisplayIcon={app}\app_icon.ico
OutputDir={#OutDir}
OutputBaseFilename={#OutputBaseFilename}
SetupIconFile={#IconFile}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional icons:"

[Files]
Source: "{#BuildDir}\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\app_icon.ico"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; IconFilename: "{app}\app_icon.ico"; Tasks: desktopicon

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "Launch {#MyAppFullName}"; Flags: nowait postinstall skipifsilent
