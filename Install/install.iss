#define BufAppVersion "0.2.3"

[Setup]
AppId={{5D0F69A8-6A08-440E-AFC6-E809B1B82C88}
AppName=bufusb
AppVersion={#BufAppVersion}
AppPublisher=Bryson Kelly
AppPublisherURL=https://github.com/brysonak/bufusb
AppSupportURL=https://github.com/brysonak/bufusb/issues
AppUpdatesURL=https://github.com/brysonak/bufusb/releases
DefaultDirName={autopf}\bufusb
DefaultGroupName=bufusb
DisableProgramGroupPage=yes
LicenseFile=..\LICENSE
VersionInfoVersion={#BufAppVersion}
VersionInfoCompany=Bryson Kelly
VersionInfoDescription=bufusb bootable USB flasher installer
VersionInfoCopyright=Bryson Kelly
OutputDir=..\install
OutputBaseFilename=bufusb-setup

Compression=lzma2
SolidCompression=yes

PrivilegesRequired=lowest
PrivilegesRequiredOverridesAllowed=dialog
ArchitecturesInstallIn64BitMode=x64compatible

ChangesEnvironment=yes
WizardStyle=modern
SetupIconFile=..\buf-cli\logo.ico
UninstallDisplayIcon={app}\logo.ico


[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"


[Files]
Source: "..\target\release\bufusb.exe"; DestDir: "{app}"; DestName: "bufusb.exe"; Flags: ignoreversion
Source: "..\buf-cli\logo.ico"; DestDir: "{app}"; Flags: ignoreversion


[Icons]
Name: "{group}\bufusb"; Filename: "{app}\bufusb.exe"; IconFilename: "{app}\logo.ico"
Name: "{group}\Uninstall bufusb"; Filename: "{uninstallexe}"


[Code]

const
  EnvironmentKey = 'Environment';
  WM_SETTINGCHANGE = $001A;
  SMTO_ABORTIFHUNG = $0002;

function SendMessageTimeout(
  hWnd: LongInt;
  Msg: LongWord;
  wParam: LongInt;
  lParam: LongInt;
  fuFlags: LongWord;
  uTimeout: LongWord;
  var lpdwResult: LongWord
): LongWord;
  external 'SendMessageTimeoutW@user32.dll stdcall';

procedure RefreshEnvironment;
var
  MsgResult: LongWord;
begin
  SendMessageTimeout(
    HWND_BROADCAST,
    WM_SETTINGCHANGE,
    0,
    0,
    SMTO_ABORTIFHUNG,
    5000,
    MsgResult
  );
end;

function PathContains(Path, Dir: string): Boolean;
begin
  Result := Pos(';' + Lowercase(Dir) + ';',
    ';' + Lowercase(Path) + ';') > 0;
end;

procedure AddToPath(Dir: string);
var
  Path: string;
begin
  if not RegQueryStringValue(HKCU, EnvironmentKey, 'Path', Path) then
    Path := '';

  if not PathContains(Path, Dir) then
  begin
    if (Path <> '') and (Path[Length(Path)] <> ';') then
      Path := Path + ';';

    Path := Path + Dir;

    RegWriteExpandStringValue(HKCU, EnvironmentKey, 'Path', Path);
  end;
end;

procedure RemoveFromPath(const Dir: string);
var
  Path, NewPath, Entry: string;
  PosSep: Integer;
begin
  if not RegQueryStringValue(HKCU, EnvironmentKey, 'Path', Path) then
    Exit;

  NewPath := '';
  while Path <> '' do
  begin
    PosSep := Pos(';', Path);
    if PosSep = 0 then
    begin
      Entry := Path;
      Path := '';
    end
    else
    begin
      Entry := Copy(Path, 1, PosSep - 1);
      Delete(Path, 1, PosSep);
    end;

    if CompareText(Entry, Dir) <> 0 then
    begin
      if NewPath <> '' then
        NewPath := NewPath + ';';
      NewPath := NewPath + Entry;
    end;
  end;

  RegWriteExpandStringValue(HKCU, EnvironmentKey, 'Path', NewPath);
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  if CurStep = ssPostInstall then
  begin
    AddToPath(ExpandConstant('{app}'));
    RefreshEnvironment;
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
  begin
    RemoveFromPath(ExpandConstant('{app}'));
    RefreshEnvironment;
  end;
end;
