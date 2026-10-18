#ifndef Payload
  #error Payload is required
#endif
[Setup]
AppId=KotorinToys.VoiceMixerSpkRouter
AppName=VoiceMixerSpkRouter
AppVersion={#ProductVersion}
DefaultDirName={localappdata}\Programs\VoiceMixerSpkRouter
DefaultGroupName=VoiceMixerSpkRouter
PrivilegesRequired=lowest
ArchitecturesAllowed=x64os
ArchitecturesInstallIn64BitMode=x64os
MinVersion=10.0.22000
OutputDir={#OutputDirectory}
OutputBaseFilename=VoiceMixerSpkRouter-{#ProductVersion}-setup
Compression=lzma2/fast
SolidCompression=yes
CloseApplications=no
RestartApplications=no
AllowNoIcons=yes
DisableProgramGroupPage=yes
UninstallDisplayIcon={app}\VoiceMixerSpkRouter.exe,0
SetupIconFile=..\assets\SpkRouter.ico
WizardStyle=modern
[Languages]
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"
[Files]
Source: "{#Payload}\maintenance\*"; DestDir: "{tmp}\spk-maintenance"; Flags: dontcopy recursesubdirs
Source: "{#Payload}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs; Excludes: "*.pdb,spk-settings.json,spk-status.json,spk-instance.lock"
[Icons]
Name: "{userprograms}\VoiceMixerSpkRouter"; Filename: "{app}\VoiceMixerSpkRouter.exe"; WorkingDir: "{app}"
[Run]
Filename: "{app}\VoiceMixerSpkRouter.exe"; Description: "VoiceMixerSpkRouterを起動"; Flags: nowait postinstall skipifsilent
[Code]
var MaintenancePrepared, RemovalCompleted: Boolean;
function Maintain(Operation: String): Boolean;
var Code: Integer; Helper, Args: String;
begin
  Helper := ExpandConstant('{app}\maintenance\SpkRouter.Maintenance.exe');
  Args := '--product VoiceMixerSpkRouter --root "' + ExpandConstant('{app}') + '/" --operation ' + Operation;
  Result := FileExists(Helper) and Exec(Helper, Args, ExpandConstant('{app}'), SW_HIDE, ewWaitUntilTerminated, Code);
  if Result then Result := Code = 0;
end;
function PrepareToInstall(var NeedsRestart: Boolean): String;
var Code: Integer; Helper, Args: String;
begin
  Result := '';
  if FileExists(ExpandConstant('{app}\VoiceMixerSpkRouter.exe')) then begin
    ExtractTemporaryFiles('{tmp}\spk-maintenance\*');
    Helper := ExpandConstant('{tmp}\spk-maintenance\SpkRouter.Maintenance.exe');
    Args := '--product VoiceMixerSpkRouter --root "' + ExpandConstant('{app}') + '/" --operation update';
    if not Exec(Helper, Args, ExpandConstant('{app}'), SW_HIDE, ewWaitUntilTerminated, Code) then Code := -1;
    if Code <> 0 then Result := '正常終了または製品識別を確認できないため更新を中止しました。設定画面を閉じ、状態を確認してください。';
  end;
end;
function InitializeUninstall: Boolean;
begin
  Result := False;
  if not UninstallSilent then
    if MsgBox('VoiceMixerSpkRouterを削除します。設定は保持します。', mbConfirmation, MB_YESNO) <> IDYES then Exit;
  Result := Maintain('remove');
  MaintenancePrepared := Result;
  if (not Result) and (not UninstallSilent) then
    MsgBox('正常終了または管理予約の整理を確認できないため削除を中止しました。', mbError, MB_OK);
end;
procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usDone then RemovalCompleted := True;
end;
procedure DeinitializeUninstall;
begin
  if MaintenancePrepared and (not RemovalCompleted) and FileExists(ExpandConstant('{app}\VoiceMixerSpkRouter.exe')) then
    if not Maintain('recover') then Log('削除取消後の予約復元を確認できません。管理設定を確認してください。');
end;
