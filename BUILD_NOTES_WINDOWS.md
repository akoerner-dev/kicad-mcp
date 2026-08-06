# Windows-Build-Notizen (GNU-Toolchain)

Kurzrezept, mit dem `cargo check -p konnect-core` auf Windows ohne Visual Studio
durchläuft. Getestet 2026-08-06.

## Benötigte Tools (via winget)

```powershell
winget install Rustlang.Rustup --source winget
winget install Kitware.CMake --source winget
winget install Google.Protobuf --source winget
winget install BrechtSanders.WinLibs.POSIX.MSVCRT --source winget
rustup toolchain install stable-x86_64-pc-windows-gnu
```

Die GNU-Toolchain vermeidet den MSVC-Linker (kein Visual Studio nötig); WinLibs
liefert `gcc`/`dlltool`/`ld` (MSVCRT-Variante, passt zum `-pc-windows-gnu`-Target).

## Bauen

`cmake`, `protoc` und der WinLibs-`mingw64\bin`-Ordner müssen in `PATH` sein, und:

```powershell
$env:PROTOC = "<...>\Google.Protobuf...\bin\protoc.exe"
# nng-sys' gebündeltes C bricht mit GCC >=14, weil incompatible-pointer-types
# jetzt ein Fehler ist -> auf Warnung zurückstufen:
$env:CFLAGS   = "-Wno-error=incompatible-pointer-types -Wno-incompatible-pointer-types"
$env:CXXFLAGS = $env:CFLAGS

cargo +stable-x86_64-pc-windows-gnu check -p konnect-core --lib
```

## Hinweis

Dies prüft nur `konnect-core` (enthält den DRC-Patch). Der volle Workspace zieht
zusätzlich Tauri (schematic-viewer) — dafür braucht es WebView2 und ggf. mehr.
