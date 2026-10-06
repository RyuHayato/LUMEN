@echo off
rem ---------------------------------------------------------------------------
rem  LUMEN launcher - double-click this.
rem
rem  No terminal window is left behind, and `cargo run` is never needed.
rem
rem  Why a release build matters: src-tauri/src/main.rs carries
rem      #![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
rem  A DEBUG build is therefore a *console* application and always drags a
rem  terminal window along with it. A RELEASE build is a GUI application with no
rem  console subsystem at all. That is the whole trick, and it costs nothing.
rem
rem  The binary is rebuilt only when it is missing or older than the sources, so
rem  after the first build this is instant and silent.
rem ---------------------------------------------------------------------------

setlocal
set "ROOT=%~dp0"
set "EXE=%ROOT%target\release\lumen-app.exe"

rem Ask PowerShell whether a rebuild is needed: the binary is stale if it is
rem missing, or if any source file is newer than it.
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$exe = '%EXE%'; $root = '%ROOT%'; if (-not (Test-Path $exe)) { exit 1 }; $t = (Get-Item $exe).LastWriteTimeUtc; $newer = Get-ChildItem -Path \"$root\src-tauri\src\",\"$root\crates\core\src\",\"$root\ui\" -Recurse -File -Include *.rs,*.js,*.css,*.html,*.toml,*.json,*.sql -ErrorAction SilentlyContinue | Where-Object { $_.LastWriteTimeUtc -gt $t } | Select-Object -First 1; if ($newer) { exit 1 } else { exit 0 }"

if errorlevel 1 goto :build
goto :run

:build
echo LUMEN needs building once. This takes a couple of minutes...
pushd "%ROOT%"
cargo build --release --workspace
if errorlevel 1 (
    popd
    echo.
    echo Build FAILED - errors are above.
    pause
    exit /b 1
)
popd

:run
if not exist "%EXE%" (
    echo Could not find "%EXE%".
    pause
    exit /b 1
)
start "" "%EXE%"
exit /b 0