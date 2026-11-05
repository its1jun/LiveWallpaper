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

rem ffmpeg.exe: if libs\ffmpeg.exe was not provided, fetch the one bundled in the pip package
rem imageio-ffmpeg (same source the Python version uses) and put it next to the exe in dist.
if exist "builder\dist\LiveWallpaper\libs\ffmpeg.exe" goto :ffmpeg_ok
echo [ffmpeg] libs\ffmpeg.exe not found. Fetching it from the imageio-ffmpeg pip package...
python -m pip install --quiet --disable-pip-version-check imageio-ffmpeg
if errorlevel 1 goto :ffmpeg_warn
python -c "import imageio_ffmpeg,shutil,sys; shutil.copyfile(imageio_ffmpeg.get_ffmpeg_exe(), sys.argv[1])" "builder\dist\LiveWallpaper\libs\ffmpeg.exe"
if errorlevel 1 goto :ffmpeg_warn
goto :ffmpeg_ok
:ffmpeg_warn
echo [WARNING] ffmpeg.exe could not be bundled. Static wallpaper / GIF / optimization will not work.
echo           Install Python + "pip install imageio-ffmpeg", or put ffmpeg.exe in libs\ and rebuild.
:ffmpeg_ok
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
