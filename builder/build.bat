@echo off
chcp 65001 >nul
setlocal
rem ============================================================
rem LiveWallpaper Rust build: Cargo -> portable folder (builder\dist\LiveWallpaper)
rem Installer: run build_installer.bat separately afterwards.
rem Prerequisite: Rust toolchain, libs\libmpv-2.dll (or mpv-2.dll), builder\icon.ico
rem Optional: libs\ffmpeg.exe for GIF/WebP conversion, video optimization and snapshots.
rem ============================================================
cd /d "%~dp0.."

if not exist "libs\libmpv-2.dll" if not exist "libs\mpv-2.dll" (
    echo [ERROR] Put libmpv-2.dll or mpv-2.dll in libs\.
    goto :fail
)
if not exist "builder\icon.ico" (
    echo [ERROR] builder\icon.ico is missing.
    goto :fail
)
where cargo >nul 2>nul
if errorlevel 1 (
    echo [ERROR] Rust/Cargo is not installed. Install it from https://rustup.rs/.
    goto :fail
)

echo [1/1] Building LiveWallpaper.exe...
cargo build --release
if errorlevel 1 goto :fail

if exist "builder\dist\LiveWallpaper" rmdir /s /q "builder\dist\LiveWallpaper"
mkdir "builder\dist\LiveWallpaper"
copy /y "target\release\LiveWallpaper.exe" "builder\dist\LiveWallpaper\LiveWallpaper.exe" >nul
if errorlevel 1 goto :fail
xcopy /e /i /y /q "libs" "builder\dist\LiveWallpaper\libs" >nul
if errorlevel 1 goto :fail
copy /y "builder\icon.ico" "builder\dist\LiveWallpaper\icon.ico" >nul
if errorlevel 1 goto :fail
if exist "README.md" copy /y "README.md" "builder\dist\LiveWallpaper\README.md" >nul

echo Build finished. Portable folder: builder\dist\LiveWallpaper\
pause
exit /b 0

:fail
echo.
echo Build failed. See the message above.
pause
exit /b 1
