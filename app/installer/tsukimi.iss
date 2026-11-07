; Tsukimi's Windows setup (Inno Setup 6): the usual wizard - Welcome, License, Folder, Shortcuts,
; Install, Finish (with "Launch Tsukimi") - per user, no administrator rights, with its uninstaller
; (Settings -> Apps and the Start menu). The release workflow builds it:
;
;   iscc /DAppVersion=4.1.0 /DExe=<path>\tsukimi.exe /O<out dir> app\installer\tsukimi.iss
;
; This setup is also the update: it is the only Windows file of a release. Tsukimi's Update button
; (crates/tsukimi-core/src/updater.rs) downloads it and starts it on the install's folder:
;
;   Tsukimi-setup.exe /SILENT /SUPPRESSMSGBOXES /NORESTART /CLOSEAPPLICATIONS /LAUNCH=1 /DIR=<folder> /LOG=<file>
;
; Before installing, an existing install is uninstalled silently (program, shortcuts, Apps entry -
; the library, settings and downloads in %APPDATA% / %LOCALAPPDATA%\akulav\Tsukimi stay; only an
; uninstall where you answer "yes" deletes them), then the new version is installed and, with
; /LAUNCH=1, started. An install of dango (the app's previous name, Tauri's NSIS setup) is removed
; the same way; Tsukimi moves dango's library and downloads over on its first start.
; The pictures come from scripts\installer-images.ps1.

#ifndef AppVersion
  #define AppVersion "0.0.0"
#endif
#ifndef Exe
  #define Exe "..\..\target\release\tsukimi.exe"
#endif
; (tests compile a separate copy - /DName=TsukimiTest /DGuid=... - so a test install never touches a real one)
#ifndef Name
  #define Name "Tsukimi"
#endif
#ifndef Guid
  ; never change it: it is how an update or the uninstaller finds this install
  #define Guid "4E0B7C2A-6F1D-4B8E-9C3A-7D25E81F0A64"
#endif
; (Windows file versions are numbers only: 1.0.0-beta.1 -> 1.0.0)
#define NumVersion Copy(AppVersion, 1, Pos("-", AppVersion + "-") - 1)

[Setup]
AppId={{{#Guid}}
AppName={#Name}
AppVersion={#AppVersion}
AppVerName=Tsukimi {#AppVersion}
AppPublisher=Akulav
AppCopyright=Made by Akulav
AppPublisherURL=https://github.com/Akulav/local-anime
AppSupportURL=https://github.com/Akulav/local-anime/issues
AppUpdatesURL=https://github.com/Akulav/local-anime/releases
VersionInfoCompany=Akulav
VersionInfoCopyright=Made by Akulav
VersionInfoProductName=Tsukimi
VersionInfoDescription=Tsukimi Setup
VersionInfoVersion={#NumVersion}
DefaultDirName={localappdata}\Programs\Tsukimi
; (Inno Setup hides the Welcome and folder pages unless told: this setup shows both)
DisableWelcomePage=no
DisableDirPage=no
DisableProgramGroupPage=yes
UsePreviousAppDir=yes
DirExistsWarning=no
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
LicenseFile=..\..\LICENSE
OutputBaseFilename=Tsukimi-setup
SetupIconFile=..\icons\icon.ico
UninstallDisplayIcon={app}\tsukimi.exe
UninstallDisplayName={#Name}
WizardStyle=modern
WizardImageFile=wizard-large.bmp,wizard-large-200.bmp
WizardSmallImageFile=wizard-small.bmp,wizard-small-200.bmp
Compression=lzma2/max
SolidCompression=yes
; (a running Tsukimi is closed before its program is replaced)
CloseApplications=force
RestartApplications=no

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"

[CustomMessages]
AppComment=Fast desktop anime player - made by Akulav
DeleteData=Tsukimi is uninstalled.%n%nAlso delete your library, watch progress, settings and downloaded episodes?%n%n%1%n%2%n%nNo keeps them: installing Tsukimi again brings everything back.

[Tasks]
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"

[Files]
Source: "{#Exe}"; DestDir: "{app}"; DestName: "tsukimi.exe"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#Name}"; Filename: "{app}\tsukimi.exe"; WorkingDir: "{app}"; Comment: "{cm:AppComment}"
Name: "{autoprograms}\{cm:UninstallProgram,{#Name}}"; Filename: "{uninstallexe}"; WorkingDir: "{app}"
Name: "{autodesktop}\{#Name}"; Filename: "{app}\tsukimi.exe"; WorkingDir: "{app}"; Comment: "{cm:AppComment}"; Tasks: desktopicon

[Run]
Filename: "{app}\tsukimi.exe"; Description: "{cm:LaunchProgram,Tsukimi}"; Flags: nowait postinstall skipifsilent
; (an update from Tsukimi's Update button: start the new version when the silent install is done)
Filename: "{app}\tsukimi.exe"; Parameters: "--after-update"; WorkingDir: "{app}"; Flags: nowait; Check: LaunchAfterUpdate

[UninstallDelete]
Type: files; Name: "{app}\tsukimi.exe.old"

[Code]
const
  DangoKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\dango';

// A running Tsukimi from this folder is ended first.
procedure StopRunning(Dir: String);
var
  Code: Integer;
begin
  Exec('powershell.exe',
       '-NoProfile -NonInteractive -Command "Get-Process tsukimi -ErrorAction SilentlyContinue | ' +
       'Where-Object { $_.Path -eq ''' + Dir + '\tsukimi.exe'' } | Stop-Process -Force"',
       '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(1000);
end;

// /LAUNCH=1 (an update started by Tsukimi): start Tsukimi when the silent install is done.
function LaunchAfterUpdate(): Boolean;
begin
  Result := WizardSilent and (ExpandConstant('{param:LAUNCH|0}') = '1');
end;

// dango (the previous name, installed by Tauri's NSIS setup into %LOCALAPPDATA%\dango): its program,
// shortcuts and Apps entry go. Its data is NOT touched here (the old cache folder - with the
// downloads - sits inside the same folder): Tsukimi moves it over on its first start.
procedure RemoveDango();
var
  Dir: String;
  Code: Integer;
begin
  if not RegQueryStringValue(HKCU, DangoKey, 'InstallLocation', Dir) then
    Dir := ExpandConstant('{localappdata}\dango');
  Dir := RemoveQuotes(Dir);
  if not FileExists(Dir + '\dango.exe') and not RegKeyExists(HKCU, DangoKey) then
    exit;
  Log('Removing dango (the previous name of Tsukimi) from ' + Dir);
  Exec('powershell.exe',
       '-NoProfile -NonInteractive -Command "Get-Process dango -ErrorAction SilentlyContinue | Stop-Process -Force"',
       '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(500);
  DeleteFile(Dir + '\dango.exe');
  DeleteFile(Dir + '\uninstall.exe');
  DeleteFile(ExpandConstant('{userprograms}\dango.lnk'));
  DeleteFile(ExpandConstant('{userdesktop}\dango.lnk'));
  RegDeleteKeyIncludingSubkeys(HKCU, DangoKey);
  RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Akulav\dango');
  RemoveDir(Dir); // only when empty
end;

// An install that is already there is uninstalled first, silently: the old program, its shortcuts and
// its Apps entry go; the library and downloads stay (deleted only when the user asks, never when silent).
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Uninst: String;
  Code: Integer;
begin
  Result := '';
  StopRunning(ExpandConstant('{app}'));
  if RegQueryStringValue(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\{' + '{#Guid}' + '}_is1',
                         'UninstallString', Uninst) then
  begin
    Uninst := RemoveQuotes(Uninst);
    if FileExists(Uninst) then
    begin
      Log('Uninstalling the installed version first (library and downloads stay): ' + Uninst);
      if not Exec(Uninst, '/VERYSILENT /SUPPRESSMSGBOXES /NORESTART', '', SW_HIDE, ewWaitUntilTerminated, Code) then
        Log('The old uninstaller did not start; installing over it');
    end;
  end;
  RemoveDango();
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Roaming, Local: String;
begin
  if CurUninstallStep = usUninstall then
    StopRunning(ExpandConstant('{app}'));
  if (CurUninstallStep = usPostUninstall) and not UninstallSilent then
  begin
    Roaming := ExpandConstant('{userappdata}\akulav\Tsukimi');
    Local := ExpandConstant('{localappdata}\akulav\Tsukimi');
    if (DirExists(Roaming) or DirExists(Local)) and
       (MsgBox(FmtMessage(CustomMessage('DeleteData'), [Roaming, Local]), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES) then
    begin
      DelTree(Roaming, True, True, True);
      DelTree(Local, True, True, True);
      RemoveDir(ExpandConstant('{userappdata}\akulav'));
      RemoveDir(ExpandConstant('{localappdata}\akulav'));
    end;
  end;
end;
