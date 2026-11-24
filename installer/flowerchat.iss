; The Windows installer, for Inno Setup 6. It installs what bundle.ps1 has
; gathered in target\bundle, so run that first:
;
;   cargo build --release -p flowerchat-app -p flowerchat-client
;   pwsh installer\bundle.ps1
;   iscc /DVersion=0.1.0 installer\flowerchat.iss
;
; The result is target\installer\flowerchat-VERSION-setup.exe.

#ifndef Version
  #define Version "0.0.0"
#endif

[Setup]
; Tells Windows that a later version is the same program. Never change it.
AppId={{05170068-5380-43D5-B029-F2D99FF3D48D}
AppName=flowerchat
AppVersion={#Version}
DefaultDirName={autopf}\flowerchat
DefaultGroupName=flowerchat
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\bin\flowerchat.exe
; Installing for everyone lets the firewall be told about flowerchat, which
; takes an administrator. `/CURRENTUSER` on the command line installs for
; one person without one, and leaves the firewall to ask on first use.
PrivilegesRequired=admin
PrivilegesRequiredOverridesAllowed=commandline
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
; An update closes flowerchat if it is running.
CloseApplications=yes
RestartApplications=no
Compression=lzma2/max
SolidCompression=yes
WizardStyle=modern
SourceDir=..\target\bundle
OutputDir=..\installer
OutputBaseFilename=flowerchat-{#Version}-setup

[Files]
Source: "*"; DestDir: "{app}"; Flags: recursesubdirs ignoreversion

[InstallDelete]
; What an older version brought that this one does not.
Type: filesandordirs; Name: "{app}\bin"
Type: filesandordirs; Name: "{app}\lib"
Type: filesandordirs; Name: "{app}\libexec"

[Icons]
Name: "{group}\flowerchat"; Filename: "{app}\bin\flowerchat.exe"
Name: "{autodesktop}\flowerchat"; Filename: "{app}\bin\flowerchat.exe"; Tasks: desktopicon

[Tasks]
Name: "desktopicon"; Description: "Put a shortcut on the desktop"; Flags: unchecked

[Run]
; Calls go directly between the people in a room, so the others have to be
; able to reach flowerchat. Without a rule, Windows asks the first time,
; and a "no" there is hard to find again.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""flowerchat"""; Flags: runhidden; Check: IsAdminInstallMode
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""flowerchat"" dir=in action=allow program=""{app}\bin\flowerchat.exe"" enable=yes"; Flags: runhidden; Check: IsAdminInstallMode
Filename: "{app}\bin\flowerchat.exe"; Description: "Start flowerchat"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""flowerchat"""; Flags: runhidden; RunOnceId: "firewall"; Check: IsAdminInstallMode
