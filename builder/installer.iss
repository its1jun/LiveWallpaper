#ifndef AppVersion
  #define AppVersion "1.0.0"
#endif
#if FileExists(CompilerPath + "Languages\Korean.isl")
  #define HasKorean
#endif

[Setup]
AppId={{6A6B889A-0580-4C0E-A458-33B1742711DF}
AppName=LiveWallpaper
AppVersion={#AppVersion}
DefaultDirName={autopf}\LiveWallpaper
DefaultGroupName=LiveWallpaper
; 사용자 단위 설치: 관리자 권한 불필요 ({autopf}가 %LOCALAPPDATA%\Programs로 풀려 설정/캐시를 {app}에 쓸 수 있음, 자동 시작은 HKCU 에 저장됨)
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputDir=output
OutputBaseFilename=LiveWallpaper-Setup
SetupIconFile=icon.ico
UninstallDisplayIcon={app}\LiveWallpaper.exe
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
DisableProgramGroupPage=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
#ifdef HasKorean
Name: "korean"; MessagesFile: "compiler:Languages\Korean.isl"
#endif

[CustomMessages]
english.DeleteUserData=Also delete settings and the converted video cache?
#ifdef HasKorean
korean.DeleteUserData=설정과 변환된 영상 캐시도 삭제할까요?
#endif

[Files]
Source: "dist\LiveWallpaper\*"; DestDir: "{app}"; Flags: recursesubdirs createallsubdirs ignoreversion

[Icons]
Name: "{autoprograms}\LiveWallpaper"; Filename: "{app}\LiveWallpaper.exe"

[Registry]
; 제거 시 자동 시작 등록(앱이 HKCU\...\Run 에 직접 씀)도 함께 삭제
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: none; ValueName: "LiveWallpaper"; Flags: uninsdeletevalue dontcreatekey

[Run]
Filename: "{app}\LiveWallpaper.exe"; Description: "{cm:LaunchProgram,LiveWallpaper}"; Flags: nowait postinstall skipifsilent

[Code]
procedure KillApp;
var
  R: Integer;
begin
  { 트레이에 상주하는 앱이라 설치/제거 전에 종료해야 파일을 덮어쓰고 지울 수 있음 }
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM LiveWallpaper.exe', '', SW_HIDE, ewWaitUntilTerminated, R);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  KillApp;
  Result := '';
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usUninstall then
    KillApp;
  if CurUninstallStep = usPostUninstall then
    if MsgBox(ExpandConstant('{cm:DeleteUserData}'), mbConfirmation, MB_YESNO or MB_DEFBUTTON2) = IDYES then
    begin
      DelTree(ExpandConstant('{app}\cache'), True, True, True);
      DeleteFile(ExpandConstant('{app}\config.json'));
      DeleteFile(ExpandConstant('{app}\config.tmp'));
      RemoveDir(ExpandConstant('{app}'));
      { 이전 버전이 AppData에 남긴 데이터도 함께 정리 }
      DelTree(ExpandConstant('{userappdata}\LiveWallpaper'), True, True, True);
    end;
end;
