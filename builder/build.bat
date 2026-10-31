@echo off
chcp 65001 >nul
setlocal
rem ============================================================
rem  LiveWallpaper 빌드: PyInstaller(exe 폴더) -> Inno Setup(설치 파일)
rem  사용법: builder\build.bat 더블클릭 (프로젝트 루트의 libs\ 에 libmpv DLL이 있어야 함)
rem  결과  : builder\dist\LiveWallpaper\      (설치 없이 실행 가능한 폴더)
rem          builder\output\LiveWallpaper-Setup.exe  (Inno Setup이 있을 때)
rem ============================================================
set VERSION=1.0.0

cd /d "%~dp0.."

if not exist "libs\libmpv-2.dll" if not exist "libs\mpv-2.dll" (
    echo [오류] libs\ 폴더에 libmpv-2.dll 또는 mpv-2.dll 이 없습니다. README의 "준비"를 참고하세요.
    goto :fail
)

if not exist "builder\icon.ico" (
    echo [오류] builder\icon.ico 가 없습니다. 아이콘 파일을 builder 폴더에 넣어주세요.
    goto :fail
)

echo [1/3] 빌드 도구 설치...
python -m pip install -r requirements.txt pyinstaller
if errorlevel 1 goto :fail

echo [2/3] PyInstaller...
if exist builder\dist rmdir /s /q builder\dist
python -m PyInstaller --noconfirm --clean --windowed --icon "%CD%\builder\icon.ico" --name LiveWallpaper ^
    --distpath builder\dist --workpath builder\work --specpath builder\work ^
    --paths "%CD%\src" --collect-all imageio_ffmpeg main.py
if errorlevel 1 goto :fail

rem libmpv DLL은 exe 옆 libs\ 에 둔다 (player.py가 이 위치를 찾음)
xcopy /e /i /y /q libs builder\dist\LiveWallpaper\libs >nul
if errorlevel 1 goto :fail

rem 트레이/설정 창 아이콘은 실행 중에 exe 옆의 icon.ico를 읽는다 (config.icon_path)
copy /y builder\icon.ico builder\dist\LiveWallpaper\icon.ico >nul
if errorlevel 1 goto :fail

echo [3/3] Inno Setup...
set "ISCC="
for %%P in ("%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe" "%ProgramFiles%\Inno Setup 6\ISCC.exe" "%LocalAppData%\Programs\Inno Setup 6\ISCC.exe") do if exist "%%~P" set "ISCC=%%~P"
if not defined ISCC for /f "delims=" %%I in ('where ISCC.exe 2^>nul') do set "ISCC=%%I"
if not defined ISCC (
    echo [건너뜀] Inno Setup 6을 찾지 못했습니다. 설치 파일은 만들지 않았습니다.
    echo          폴더 배포: builder\dist\LiveWallpaper\
    goto :done
)
"%ISCC%" /Qp /DAppVersion=%VERSION% builder\installer.iss
if errorlevel 1 goto :fail
echo 설치 파일: builder\output\LiveWallpaper-Setup.exe

:done
echo 완료.
pause
exit /b 0

:fail
echo.
echo 빌드 실패.
pause
exit /b 1
