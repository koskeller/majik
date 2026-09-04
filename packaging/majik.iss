; Inno Setup script for the Majik installer, driven by script/bundle-windows.ps1.
;
; AppId is the installer's identity forever: Windows uses it to decide whether a new installer
; upgrades an existing install or sits beside it. Changing it orphans every install that came
; before, which is why config::tests::the_installer_app_id_is_never_changed pins this literal —
; the Windows counterpart of stable_dirs_are_exactly_the_shipped_ones.
;
; The doubled leading brace is Inno's escape for a literal "{": without it the [Setup] parser reads
; the expanded GUID as a constant and aborts with `Unknown constant "92561171-..."`. The value the
; installer ends up with is the single-braced GUID, which is what must never change.
#define AppId "{{92561171-E8BA-4C40-BC5E-9A8C3191D8D3}"

#ifndef Version
  #define Version "0.0.0"
#endif
#ifndef Arch
  #define Arch "x86_64"
#endif
#ifndef SourceDir
  #define SourceDir "..\target\inno\x86_64"
#endif
#ifndef OutputDir
  #define OutputDir "..\target"
#endif

[Setup]
AppId={#AppId}
AppName=Majik
AppVersion={#Version}
AppVerName=Majik {#Version}
VersionInfoVersion={#Version}
AppPublisher=Majik
DefaultDirName={autopf}\Majik
DefaultGroupName=Majik
DisableProgramGroupPage=yes
; Per-user install: no UAC prompt, which matters while the installer is unsigned and SmartScreen is
; already asking the user to trust it once.
PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir={#OutputDir}
OutputBaseFilename=MajikSetup-{#Arch}
SetupIconFile={#SourceDir}\majik.ico
UninstallDisplayIcon={app}\majik.ico
UninstallDisplayName=Majik
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
LicenseFile={#SourceDir}\LICENSE.txt
; The auto-updater runs this installer silently while Majik is still shutting down
; (`auto_update::WINDOWS_INSTALLER_SWITCHES`); Restart Manager closes whatever is left of it so
; Majik.exe can be replaced.
CloseApplications=force

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[Tasks]
Name: "desktopicon"; Description: "Create a &desktop shortcut"; GroupDescription: "Additional shortcuts:"; Flags: unchecked

[Files]
Source: "{#SourceDir}\Majik.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\majik.ico"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\LICENSE.txt"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#SourceDir}\NOTICE.txt"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\Majik"; Filename: "{app}\Majik.exe"; IconFilename: "{app}\majik.ico"
Name: "{autodesktop}\Majik"; Filename: "{app}\Majik.exe"; IconFilename: "{app}\majik.ico"; Tasks: desktopicon

[UninstallDelete]
; The installer the auto-updater staged for a restart (see below).
Type: filesandordirs; Name: "{app}\updates"

[Run]
Filename: "{app}\Majik.exe"; Description: "Launch Majik"; Flags: nowait postinstall skipifsilent
; A silent update relaunches Majik itself, unless the app was quitting for good.
Filename: "{app}\Majik.exe"; Flags: nowait; Check: RelaunchAfterUpdate

[Code]
// The auto-updater's custom switches: `/update=true` marks a silent re-install started by the
// running app, `/relaunch=false` one started as the app quit (docs/updates.md).
function SwitchHasValue(Name: string; Value: string): Boolean;
begin
  Result := CompareText(ExpandConstant('{param:' + Name + '}'), Value) = 0;
end;

function IsUpdating(): Boolean;
begin
  Result := SwitchHasValue('update', 'true') and WizardSilent();
end;

function RelaunchAfterUpdate(): Boolean;
begin
  Result := IsUpdating() and not SwitchHasValue('relaunch', 'false');
end;
