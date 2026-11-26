<#
Clippy contra Bals: graba con cada uno por turnos, en las mismas condiciones, guarda un clip cada tanto con su propia
tecla y mide desde afuera procesador, RAM, placa, memoria de video, handles, hilos, cuánto tarda en guardar y cómo sale
el clip. Deja versus.md (para leer y mostrar) y versus.json (para comparar corridas o dárselo a Claude).

Uso (con Clippy y Bals cerrados; tapa la pantalla mientras mide):
  powershell -ExecutionPolicy Bypass -File tools\versus.ps1
  powershell -ExecutionPolicy Bypass -File tools\versus.ps1 -Minutos 5 -Vueltas 2 -Ajustes fabrica

Cada programa corre con su propia carpeta de datos (no toca tus ajustes ni tus clips) y graba el monitor principal.
#>
param(
    # Clippy.exe a medir. Vacío: el target\release\clippy.exe de este repo (o el de target\fast).
    [string]$Clippy = "",
    # Bals.exe a medir. Vacío: el de la última versión publicada en github.com/sconelli24/bals (se descarga una vez).
    [string]$Bals = "",
    # Minutos que graba cada uno por vuelta.
    [int]$Minutos = 3,
    # Segundos entre clips (el primero sale cuando el buffer de 30 s está lleno).
    [int]$CadaSeg = 30,
    # "igualados": los dos a 1080p 60 fps, ~10 Mbps, sin micrófono ni cursor. "fabrica": como vienen (Bals grabando el
    # monitor, porque de fábrica solo graba juegos).
    [ValidateSet("igualados", "fabrica")][string]$Ajustes = "igualados",
    # "movimiento": una imagen que cambia en cada cuadro, como un juego. "quieto": el escritorio sin tocar.
    [ValidateSet("movimiento", "quieto")][string]$Escena = "movimiento",
    # Vueltas: con 2 o más alterna quién va primero (A B, B A...) y promedia.
    [int]$Vueltas = 1,
    # Dónde quedan los resultados (y los exe descargados, en bin\).
    [string]$Salida = "$env:USERPROFILE\Desktop\clippy-versus"
)

$ErrorActionPreference = "Stop"
. (Join-Path $PSScriptRoot "medicion.ps1")
$repo = Split-Path $PSScriptRoot -Parent
$BufferSeg = 30

Add-Type -Namespace Versus -Name Win -MemberDefinition @'
[DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
[DllImport("user32.dll")] public static extern bool PostMessage(IntPtr hwnd, uint msg, IntPtr wp, IntPtr lp);
'@
function Find-Clase([string]$clase) { [Versus.Win]::FindWindow($clase, [NullString]::Value) }

# --- Los exe -----------------------------------------------------------------------------------------------------------
function Get-UltimaVersion([string]$repoGh, [string]$asset) {
    $bin = Join-Path $Salida "bin"
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    $rel = Invoke-RestMethod -Uri "https://api.github.com/repos/$repoGh/releases/latest" -Headers @{ "User-Agent" = "versus" }
    $a = $rel.assets | Where-Object { $_.name -eq $asset } | Select-Object -First 1
    if (-not $a) { throw "La versión $($rel.tag_name) de $repoGh no trae $asset." }
    $dest = Join-Path $bin ("{0}-{1}" -f $rel.tag_name, $asset)
    if (-not (Test-Path $dest)) {
        Write-Host "Descargando $asset $($rel.tag_name) de $repoGh..."
        Invoke-WebRequest -Uri $a.browser_download_url -OutFile $dest -UseBasicParsing
    }
    $dest
}
if (-not $Clippy) {
    $Clippy = @("target\release\clippy.exe", "target\fast\clippy.exe") | ForEach-Object { Join-Path $repo $_ } |
        Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $Clippy) { throw "No hay un clippy.exe compilado: corré 'cargo build --release' o pasá -Clippy <ruta>." }
}
if (-not $Bals) { $Bals = Get-UltimaVersion "sconelli24/bals" "bals.exe" }
foreach ($exe in @($Clippy, $Bals)) { if (-not (Test-Path $exe)) { throw "No existe $exe." } }

# --- Cómo se configura, se abre y se cierra cada uno -------------------------------------------------------------------
# Los dos guardan con F9 (la tecla no cambia el rendimiento y así se aprietan igual).
function Set-ClippyConfig([string]$datos, [string]$clips) {
    $cfg = [ordered]@{ output_dir = $clips; sound_on_save = $false; hotkey_vk = 0x78; hotkey_mods = 0; seconds = $BufferSeg; capture_mode = "pantalla" }
    if ($Ajustes -eq "igualados") {
        $cfg.fps = 60; $cfg.resolution = 1080; $cfg.bitrate_mbps = 10; $cfg.codec = "h264"
        $cfg.game_audio = $true; $cfg.desktop_audio = $true; $cfg.mic_audio = $false; $cfg.cursor = $false
    }
    $dir = Join-Path $datos "Clippy"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    [IO.File]::WriteAllText((Join-Path $dir "config.json"), ($cfg | ConvertTo-Json))
}
function Set-BalsConfig([string]$datos, [string]$clips) {
    $lines = @(
        "save_clip_hotkey = `"F9`"",
        "capture_mode = `"ManualMonitor`"",
        "replay_duration = `"Sec$BufferSeg`"",
        "sound_feedback = false",
        "auto_update = false",
        "clips_dir = '$clips'"
    )
    if ($Ajustes -eq "igualados") {
        $lines += @("resolution = `"P1080`"", "fps = `"Fps60`"", "quality = `"Balanced`"", "encoder_pref = `"HardwareGpu`"",
            "audio_capture_mode = `"FullDesktop`"", "mic_enabled = false", "capture_cursor = false")
    }
    $dir = Join-Path $datos "Bals"
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    [IO.File]::WriteAllText((Join-Path $dir "config.toml"), ($lines -join "`r`n"))
}
$apps = @(
    @{ Nombre = "Clippy"; Exe = $Clippy; Args = "--background"; Clase = "ClippyHostWindow"; Cerrar = 0x0010; Config = ${function:Set-ClippyConfig} },
    # WM_APP + 13 = WM_BALS_SHUTDOWN (src/ipc.rs de Bals): sale como desde la bandeja.
    @{ Nombre = "Bals"; Exe = $Bals; Args = "--tray"; Clase = "BalsTrayWindowClass"; Cerrar = 0x8000 + 13; Config = ${function:Set-BalsConfig} }
)
foreach ($a in $apps) {
    if ((Find-Clase $a.Clase) -ne [IntPtr]::Zero) { throw "$($a.Nombre) está abierto: cerralo desde la bandeja (Salir) antes de medir; el versus abre el suyo." }
}
# Bals captura con Windows.Graphics.Capture, que necesita el servicio de captura de Windows. Si está frenado y no
# arranca solo, Bals no graba (CreateForMonitor 0x80070422); Clippy usa DXGI y no lo necesita.
foreach ($s in @(Get-Service -Name "CaptureService*" -ErrorAction SilentlyContinue | Where-Object { $_.Status -ne "Running" })) {
    try { Start-Service $s.Name -ErrorAction Stop } catch { Write-Host "Aviso: no se pudo arrancar $($s.Name) ($($_.Exception.Message)); Bals puede no grabar." }
}

# El último error que dejó el programa en su registro: dice por qué no grabó.
function Get-UltimoError([string]$dir) {
    $logs = @(Get-ChildItem -Path $dir -Recurse -Include salida.txt, errores.txt, clippy.log -ErrorAction SilentlyContinue)
    $l = $logs | ForEach-Object { Get-Content -Encoding UTF8 $_.FullName } | Where-Object { $_ -match "ERROR|error|falló|fallo|no se pudo" } | Select-Object -Last 1
    if ($l) { $l.Trim() } else { $null }
}

# --- El clip que acaba de guardar: un .mp4 nuevo que ya nadie escribe --------------------------------------------------
function Get-Clips([string]$dir) { @(Get-ChildItem -Path $dir -Filter *.mp4 -Recurse -ErrorAction SilentlyContinue | ForEach-Object { $_.FullName }) }
function Test-Terminado([string]$file) {
    try { $fs = [IO.File]::Open($file, "Open", "Read", "None"); $len = $fs.Length; $fs.Dispose(); $len -gt 0 } catch { $false }
}

$ffprobe = Get-Command ffprobe -ErrorAction SilentlyContinue
$shell = New-Object -ComObject Shell.Application
function Get-InfoClip([string]$file) {
    $mb = (Get-Item $file).Length / 1MB
    $info = [ordered]@{ archivo = (Split-Path $file -Leaf); mb = [Math]::Round($mb, 2); segundos = $null; mbps = $null; fps = $null; alto = $null; audio = $null }
    if ($ffprobe) {
        $j = & $ffprobe.Source -v error -count_packets -show_entries "stream=codec_type,height,nb_read_packets:format=duration" -of json $file | ConvertFrom-Json
        $v = $j.streams | Where-Object { $_.codec_type -eq "video" } | Select-Object -First 1
        $info.segundos = [Math]::Round([double]$j.format.duration, 2)
        if ($v -and $info.segundos -gt 0) { $info.fps = [Math]::Round([double]$v.nb_read_packets / $info.segundos, 1); $info.alto = [int]$v.height }
        $info.audio = [bool]($j.streams | Where-Object { $_.codec_type -eq "audio" })
    } else {
        # Sin ffprobe: la duración que muestra el Explorador (columna 27, "Duración").
        $f = $shell.Namespace((Split-Path $file -Parent)).ParseName((Split-Path $file -Leaf))
        $d = $shell.Namespace((Split-Path $file -Parent)).GetDetailsOf($f, 27)
        if ($d -match "(\d+):(\d+):(\d+)") { $info.segundos = [int]$Matches[1] * 3600 + [int]$Matches[2] * 60 + [int]$Matches[3] }
    }
    if ($info.segundos -gt 0) { $info.mbps = [Math]::Round($mb * 8 * 1.048576 / $info.segundos, 2) }
    [pscustomobject]$info
}

# Pendiente por minuto del piso de cada tramo de 30 s (sin los picos de guardar): si sube siempre, algo no se suelta.
function Get-PendientePiso($rows, [string]$field) {
    $pts = @($rows | Group-Object { [Math]::Floor($_.segundo / 30) } | ForEach-Object {
        [pscustomobject]@{ x = ($_.Name -as [double]) * 0.5; y = ($_.Group | Measure-Object $field -Minimum).Minimum } })
    if ($pts.Count -lt 3) { return $null }
    $mx = ($pts | Measure-Object x -Average).Average; $my = ($pts | Measure-Object y -Average).Average
    $num = 0.0; $den = 0.0
    foreach ($p in $pts) { $num += ($p.x - $mx) * ($p.y - $my); $den += ($p.x - $mx) * ($p.x - $mx) }
    if ($den -eq 0) { return $null }
    [Math]::Round($num / $den, 2)
}

# --- Una corrida: abre el programa, lo deja llenar el buffer, guarda clips cada tanto, lo cierra -----------------------
function Invoke-Corrida($app, [int]$vuelta, [string]$dir) {
    $datos = Join-Path $dir "datos"; $clips = Join-Path $dir "clips"
    New-Item -ItemType Directory -Force -Path $datos, $clips | Out-Null
    & $app.Config $datos $clips
    $total = $Minutos * 60
    $escenaProc = $null
    if ($Escena -eq "movimiento") {
        $escenaProc = Start-Process powershell -PassThru -WindowStyle Hidden -ArgumentList "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", "`"$(Join-Path $PSScriptRoot 'escena.ps1')`"", "-Segundos", ($total + 60)
        Start-Sleep -Seconds 2
    }

    $oldAppData = $env:APPDATA
    $env:APPDATA = $datos
    try {
        $p = Start-Process -FilePath $app.Exe -ArgumentList $app.Args -PassThru `
            -RedirectStandardOutput (Join-Path $dir "salida.txt") -RedirectStandardError (Join-Path $dir "errores.txt")
    } finally { $env:APPDATA = $oldAppData }

    $res = [ordered]@{ app = $app.Nombre; vuelta = $vuelta; carpeta = $dir; se_cayo = $false; cerro_bien = $null; cerrar_ms = $null }
    $listo = [Diagnostics.Stopwatch]::StartNew()
    while ((Find-Clase $app.Clase) -eq [IntPtr]::Zero -and -not $p.HasExited -and $listo.Elapsed.TotalSeconds -lt 20) { Start-Sleep -Milliseconds 100 }
    if ($p.HasExited -or (Find-Clase $app.Clase) -eq [IntPtr]::Zero) {
        if (-not $p.HasExited) { $p.Kill() }
        if ($escenaProc -and -not $escenaProc.HasExited) { $escenaProc.Kill() }
        throw "$($app.Nombre) no arrancó (mirá $dir\salida.txt y errores.txt)."
    }

    $med = New-Medidor $p.Id
    $rows = New-Object Collections.Generic.List[object]
    $saves = New-Object Collections.Generic.List[object]
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $nextSample = 1.0; $nextClip = $BufferSeg + 5.0; $pend = $null; $lastSaveEnd = -100.0
    Write-Host "  $($app.Nombre) (PID $($p.Id)): $Minutos min, un clip cada $CadaSeg s"
    while ($true) {
        Start-Sleep -Milliseconds 50
        $t = $clock.Elapsed.TotalSeconds
        if ($p.HasExited) { $res.se_cayo = $true; Write-Host "  ¡$($app.Nombre) se cerró solo a los $([Math]::Round($t)) s!"; break }
        if (-not $pend -and $t -ge $nextClip -and $t -lt $total) {
            $pend = @{ t0 = $t; antes = Get-Clips $clips }
            Send-Tecla 0x78 0
            $nextClip += $CadaSeg
        }
        if ($pend) {
            $nuevo = Get-Clips $clips | Where-Object { $pend.antes -notcontains $_ } | Where-Object { Test-Terminado $_ } | Select-Object -First 1
            if ($nuevo -or $t - $pend.t0 -gt 30) {
                $ms = if ($nuevo) { [Math]::Round(($t - $pend.t0) * 1000) } else { $null }
                $saves.Add([pscustomobject]@{ segundo = [Math]::Round($pend.t0, 1); guardado_ms = $ms; archivo = $nuevo })
                Write-Host ("    clip {0}: {1}" -f $saves.Count, $(if ($nuevo) { "$ms ms" } else { "no apareció en 30 s" }))
                $pend = $null; $lastSaveEnd = $t
            }
        }
        if ($t -ge $nextSample) {
            $s = Read-Medidor $med
            $fase = if ($t -lt $BufferSeg + 5) { "llenando" } elseif ($pend -or $t - $lastSaveEnd -lt 3) { "guardando" } else { "grabando" }
            $row = [pscustomobject]@{ segundo = [Math]::Round($t, 1); fase = $fase }
            foreach ($pr in $s.PSObject.Properties) { $row | Add-Member -NotePropertyName $pr.Name -NotePropertyValue $pr.Value }
            $rows.Add($row)
            $nextSample += 1
        }
        if ($t -ge $total -and -not $pend) { break }
        # Dos clips seguidos sin salir y ninguno bueno: no está grabando, no tiene sentido medirlo hasta el final.
        if ($saves.Count -ge 2 -and -not ($saves | Where-Object { $_.archivo })) {
            Write-Host "  $($app.Nombre) no está grabando: corto esta corrida."
            break
        }
    }

    if (-not $p.HasExited) {
        $h = Find-Clase $app.Clase
        $c = [Diagnostics.Stopwatch]::StartNew()
        [void][Versus.Win]::PostMessage($h, [uint32]$app.Cerrar, [IntPtr]::Zero, [IntPtr]::Zero)
        $res.cerro_bien = $p.WaitForExit(15000)
        $res.cerrar_ms = [Math]::Round($c.Elapsed.TotalMilliseconds)
        if (-not $res.cerro_bien) { $p.Kill(); $res.cerrar_ms = $null }
    }
    if ($escenaProc -and -not $escenaProc.HasExited) { $escenaProc.Kill() }
    $rows | Export-Csv -NoTypeInformation -Encoding UTF8 -Path (Join-Path $dir "muestras.csv")

    $rec = @($rows | Where-Object { $_.fase -eq "grabando" })
    $sav = @($rows | Where-Object { $_.fase -eq "guardando" })
    $full = @($rows | Where-Object { $_.fase -ne "llenando" })
    $infos = @($saves | Where-Object { $_.archivo } | ForEach-Object { Get-InfoClip $_.archivo })
    $first = $full | Select-Object -First 1; $last = $full | Select-Object -Last 1
    $res.cpu_pct = Get-Stat $rec cpu_pct
    $res.cpu_p95_pct = Get-Stat $rec cpu_pct p95
    $res.cpu_pico_guardando_pct = Get-Stat $sav cpu_pct max
    $res.ram_privada_mb = Get-Stat $rec ram_privada_mb
    $res.ram_pico_guardando_mb = Get-Stat $sav ram_privada_mb max
    $res.ram_ws_mb = Get-Stat $rec ram_ws_mb
    $res.vram_mb = Get-Stat $rec vram_mb
    $res.gpu_pct = Get-Stat $rec gpu_pct
    $res.gpu_encoder_pct = Get-Stat $rec gpu_encoder
    $res.gpu_3d_pct = Get-Stat $rec gpu_3d
    $res.gpu_conversion_pct = Get-Stat $rec gpu_conversion
    $res.gpu_copia_pct = Get-Stat $rec gpu_copia
    $res.ram_piso_mb_por_min = Get-PendientePiso $full ram_privada_mb
    $res.handles_suben = if ($first) { $last.handles - $first.handles } else { $null }
    $res.hilos_suben = if ($first) { $last.hilos - $first.hilos } else { $null }
    $res.guardado_ms = Get-Stat $saves guardado_ms
    $res.guardado_max_ms = Get-Stat $saves guardado_ms max
    $res.clips_ok = $infos.Count
    $res.clips_fallidos = @($saves | Where-Object { -not $_.archivo }).Count
    $res.clip_mb = Get-Stat $infos mb
    $res.clip_mbps = Get-Stat $infos mbps
    $res.clip_segundos = Get-Stat $infos segundos
    $res.clip_fps = Get-Stat $infos fps
    $res.clip_alto = Get-Stat $infos alto
    $res.clips = $infos
    # Sin ningún clip la corrida no midió grabar (todo quedó en "guardando"): no entra en el promedio.
    $res.no_grabo = $infos.Count -eq 0
    $res.motivo = if ($res.no_grabo) { Get-UltimoError $dir } else { $null }
    [pscustomobject]$res
}

# --- La prueba ---------------------------------------------------------------------------------------------------------
$stamp = Get-Date -Format "yyyy-MM-dd_HH-mm-ss"
$base = Join-Path $Salida "$stamp-$Ajustes-$Escena"
New-Item -ItemType Directory -Force -Path $base | Out-Null
$cpuName = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
$gpus = @(Get-CimInstance Win32_VideoController | ForEach-Object { $_.Name }) -join " + "
$vc = Get-CimInstance Win32_VideoController | Where-Object { $_.CurrentHorizontalResolution } | Select-Object -First 1
# La resolución del escritorio de verdad (WMI a veces queda con la del panel, aunque Windows esté en otra).
Add-Type -AssemblyName System.Windows.Forms
$pant = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$pc = [ordered]@{
    procesador = $cpuName; placa = $gpus
    ram_gb = [Math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB)
    windows = (Get-CimInstance Win32_OperatingSystem).Caption + " " + [Environment]::OSVersion.Version.ToString()
    monitor = "$($pant.Width)x$($pant.Height) a $($vc.CurrentRefreshRate) Hz"
}
$ver = @{ Clippy = (Get-Item $Clippy).VersionInfo.ProductVersion; Bals = (Get-Item $Bals).VersionInfo.ProductVersion }
Write-Host "Versus en $base ($Ajustes, escena $Escena, $Vueltas vuelta(s) de $Minutos min por programa)"
if ($Escena -eq "movimiento") { Write-Host "La pantalla queda tapada por la escena mientras mide (Esc la cierra, pero arruina la medición)." }

$corridas = New-Object Collections.Generic.List[object]
for ($v = 1; $v -le $Vueltas; $v++) {
    $orden = if ($v % 2 -eq 1) { $apps } else { @($apps[1], $apps[0]) }
    foreach ($a in $orden) {
        Write-Host "Vuelta $v - $($a.Nombre)"
        $corridas.Add((Invoke-Corrida $a $v (Join-Path $base "$($a.Nombre)-$v")))
        Start-Sleep -Seconds 10 # que la placa y el disco vuelvan a reposo antes del otro
    }
}

# --- Resultado: promedio de las vueltas, uno al lado del otro ----------------------------------------------------------
# Cada métrica: nombre, unidad y si gana el más bajo.
$metricas = @(
    @("cpu_pct", "Procesador grabando", "%", $true), @("cpu_p95_pct", "Procesador grabando (p95)", "%", $true),
    @("cpu_pico_guardando_pct", "Procesador, pico al guardar", "%", $true),
    @("ram_privada_mb", "RAM privada grabando", "MB", $true), @("ram_pico_guardando_mb", "RAM, pico al guardar", "MB", $true),
    @("ram_ws_mb", "RAM en uso (working set)", "MB", $true), @("vram_mb", "Memoria de video", "MB", $true),
    @("gpu_pct", "Placa (motor más usado)", "%", $true), @("gpu_encoder_pct", "Placa: encoder", "%", $true),
    @("gpu_3d_pct", "Placa: 3D", "%", $true), @("gpu_conversion_pct", "Placa: conversión de color", "%", $true),
    @("gpu_copia_pct", "Placa: copia", "%", $true),
    @("guardado_ms", "Hasta tener el clip", "ms", $true), @("guardado_max_ms", "Hasta tener el clip (peor)", "ms", $true),
    @("clips_fallidos", "Clips que no salieron", "", $true), @("clip_fps", "Cuadros por segundo del clip", "fps", $false),
    @("clip_mbps", "Bitrate del clip", "Mbps", $null), @("clip_mb", "Peso del clip", "MB", $null),
    @("clip_segundos", "Duración del clip", "s", $null), @("clip_alto", "Alto del clip", "px", $null),
    @("ram_piso_mb_por_min", "RAM: cuánto sube el piso por minuto", "MB/min", $true),
    @("handles_suben", "Handles: cuánto suben", "", $true), @("hilos_suben", "Hilos: cuánto suben", "", $true),
    @("cerrar_ms", "Tarda en cerrarse", "ms", $true)
)
function Get-Promedio([string]$app, [string]$field) { Get-Stat @($corridas | Where-Object { $_.app -eq $app -and -not $_.no_grabo }) $field }
$sinDatos = @($apps | Where-Object { $n = $_.Nombre; -not ($corridas | Where-Object { $_.app -eq $n -and -not $_.no_grabo }) } | ForEach-Object { $_.Nombre })
$promedio = [ordered]@{}
foreach ($a in $apps) { $promedio[$a.Nombre] = [ordered]@{}; foreach ($m in $metricas) { $promedio[$a.Nombre][$m[0]] = Get-Promedio $a.Nombre $m[0] } }

$md = New-Object Collections.Generic.List[string]
$md.Add("# Clippy contra Bals")
$md.Add("")
$md.Add("$stamp · ajustes **$Ajustes** · escena **$Escena** · $Vueltas vuelta(s) de $Minutos min por programa · un clip cada $CadaSeg s")
$md.Add("")
$md.Add("- Clippy $($ver.Clippy): ``$Clippy``")
$md.Add("- Bals $($ver.Bals): ``$Bals``")
$md.Add("- PC: $($pc.procesador) · $($pc.placa) · $($pc.ram_gb) GB · $($pc.windows) · monitor $($pc.monitor)")
$md.Add("")
foreach ($n in $sinDatos) {
    $md.Add("> **$n no grabó en ninguna vuelta**: no hay con qué comparar (ver ""Cada corrida""). La tabla no tiene ganador.")
    $md.Add("")
}
$md.Add("| | Clippy | Bals | Diferencia | Mejor |")
$md.Add("|---|---:|---:|---:|---|")
$gana = @{ Clippy = 0; Bals = 0 }
foreach ($m in $metricas) {
    $c = $promedio.Clippy[$m[0]]; $b = $promedio.Bals[$m[0]]
    if ($c -eq $null -and $b -eq $null) { continue }
    $u = if ($m[2]) { " $($m[2])" } else { "" }
    $diff = ""; $best = ""
    if ($c -ne $null -and $b -ne $null -and -not $sinDatos) {
        if ($b -ne 0) { $diff = "{0:+0;-0;0} %" -f (($c - $b) / [Math]::Abs($b) * 100) }
        if ($m[3] -ne $null -and $c -ne $b) {
            $best = if (($c -lt $b) -eq $m[3]) { "Clippy" } else { "Bals" }
            $gana[$best]++
        }
    }
    $md.Add("| $($m[1]) | $c$u | $b$u | $diff | $best |")
}
$md.Add("")
if (-not $sinDatos) {
    $md.Add("Gana en más filas: **$(if ($gana.Clippy -gt $gana.Bals) { 'Clippy' } elseif ($gana.Bals -gt $gana.Clippy) { 'Bals' } else { 'empate' })** (Clippy $($gana.Clippy), Bals $($gana.Bals)).")
    $md.Add("")
}
$md.Add("## Cómo leerlo")
$md.Add("")
$md.Add("- Todo se mide desde afuera, por proceso: lo que usa cada programa, no la PC entera. Procesador en % de toda la PC.")
$md.Add("- Bitrate, peso y alto no tienen ganador: más bitrate es mejor imagen y más disco. Con ajustes ``igualados`` los dos")
$md.Add("  apuntan a ~10 Mbps, pero Bals usa calidad variable (QVBR) y Clippy bitrate fijo: si el bitrate real difiere mucho,")
$md.Add("  el que pesa más también tiene más trabajo en el encoder.")
$md.Add("- Una diferencia chica (pocos %) puede ser ruido: repetí con ``-Vueltas 2`` o más.")
$md.Add('- Las filas de "suben" son de una corrida corta: para fugas de Clippy usá `--test-soak` (ver el README).')
if (-not $ffprobe) { $md.Add("- Sin ``ffprobe`` en el PATH no se miden los cuadros por segundo ni el alto del clip (``winget install ffmpeg``).") }
$md.Add("")
$md.Add("## Cada corrida")
$md.Add("")
foreach ($r in $corridas) {
    $motivo = if ($r.motivo) { ': `' + $r.motivo + '`' } else { "" }
    $estado = if ($r.se_cayo) { "**se cerró solo**" } elseif ($r.no_grabo) { "**no grabó** (fuera del promedio)$motivo" } elseif ($r.cerro_bien -eq $false) { "no se cerró en 15 s (se forzó)" } else { "ok" }
    $md.Add("- $($r.app), vuelta $($r.vuelta): $estado · CPU $($r.cpu_pct) % · RAM $($r.ram_privada_mb) MB · placa $($r.gpu_pct) % · $($r.clips_ok) clips · ``$($r.carpeta)``")
}
[IO.File]::WriteAllText((Join-Path $base "versus.md"), ($md -join "`r`n"), (New-Object Text.UTF8Encoding $true))
$json = [ordered]@{ fecha = $stamp; ajustes = $Ajustes; escena = $Escena; minutos = $Minutos; cada_seg = $CadaSeg; vueltas = $Vueltas
    versiones = $ver; exe = @{ Clippy = $Clippy; Bals = $Bals }; pc = $pc; promedio = $promedio; corridas = $corridas }
[IO.File]::WriteAllText((Join-Path $base "versus.json"), ($json | ConvertTo-Json -Depth 6), (New-Object Text.UTF8Encoding $false))

Write-Host ""
Get-Content -Encoding UTF8 (Join-Path $base "versus.md") | Select-Object -First ($metricas.Count + 12) | Write-Host
Write-Host ""
Write-Host "Informe: $(Join-Path $base 'versus.md')"
