@echo off
chcp 65001 >nul
setlocal
rem ============================================================
rem LiveWallpaper installer only: builder\installer.iss -> Inno Setup
rem Prerequisite: run build.bat first (builder\dist\LiveWallpaper must exist).
rem ============================================================
set VERSION=1.0.0
rem CLEAN_DIST: 1 = delete builder\dist after the installer was built
set CLEAN_DIST=0
cd /d "%~dp0.."

if not exist "builder\dist\LiveWallpaper\LiveWallpaper.exe" (
    echo [ERROR] builder\dist\LiveWallpaper is missing. Run build.bat first.
    goto :fail
)

echo [1/2] Looking for Inno Setup 6/7...
set "ISCC="
rem Inno Setup 6 and 7 (7 is listed last, so it wins when both are installed)
for %%P in ("%ProgramFiles(x86)%\Inno Setup 6\ISCC.exe" "%ProgramFiles%\Inno Setup 6\ISCC.exe" "%LocalAppData%\Programs\Inno Setup 6\ISCC.exe" "%ProgramFiles(x86)%\Inno Setup 7\ISCC.exe" "%ProgramFiles%\Inno Setup 7\ISCC.exe" "%LocalAppData%\Programs\Inno Setup 7\ISCC.exe") do if exist "%%~P" set "ISCC=%%~P"
if not defined ISCC for /f "delims=" %%I in ('where ISCC.exe 2^>nul') do set "ISCC=%%I"
if not defined ISCC (
    echo [ERROR] Inno Setup 6/7 was not found ^(install it, or add the folder with ISCC.exe to PATH^).
    goto :fail
)

echo [2/2] Building installer...
"%ISCC%" /Qp /DAppVersion=%VERSION% builder\installer.iss
if errorlevel 1 goto :fail
echo Installer: builder\output\LiveWallpaper-Setup.exe
if "%CLEAN_DIST%"=="1" if exist "builder\dist" rmdir /s /q "builder\dist"

echo Installer build finished.
pause
exit /b 0

:fail
echo.
echo Installer build failed. See the message above.
pause
exit /b 1
