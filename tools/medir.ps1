<#
Mide cuánto procesador, RAM y placa de video usa la grabadora de Clippy mientras graba y al guardar un clip.

Mide desde afuera (otro proceso), así que no le suma trabajo a Clippy y sirve igual con cualquier versión del exe:
para comparar, corré la misma prueba con cada versión y mirá resumen.csv.

Uso (con Clippy grabando, en la situación que quieras medir: escritorio, un juego, etc.):
  powershell -ExecutionPolicy Bypass -File tools\medir.ps1 -Nombre 0.4.6
  powershell -ExecutionPolicy Bypass -File tools\medir.ps1 -Nombre 0.4.5 -Clips 5 -Antes 30

Aprieta la tecla de guardar clip de config.json por su cuenta (guarda clips de verdad en tu carpeta de clips).
#>
param(
    # Etiqueta de la corrida (la versión, "juego", "escritorio"...): es lo que se compara en resumen.csv.
    [string]$Nombre = "prueba",
    # Clips que guarda.
    [int]$Clips = 3,
    # Segundos de grabación normal que mide antes de cada clip.
    [int]$Antes = 20,
    # Segundos que sigue midiendo después de que el clip quedó guardado.
    [int]$Despues = 5,
    # Carpeta de datos de Clippy (la de clippy.log y config.json).
    [string]$Datos = "$env:APPDATA\Clippy",
    # Dónde quedan los resultados.
    [string]$Salida = "$env:USERPROFILE\Desktop\clippy-medicion"
)

$ErrorActionPreference = "Stop"

# --- La grabadora: el proceso de Clippy que no es la ventana de ajustes ni el editor ---------------------------------
$tray = Get-CimInstance Win32_Process -Filter "Name LIKE '%clippy%.exe'" |
    Where-Object { $_.CommandLine -notmatch "--settings|--editor|--apply-update|--test" } |
    Select-Object -First 1
if (-not $tray) { throw "Clippy no está abierto: abrilo y dejalo grabando antes de medir." }
$clippyPid = [int]$tray.ProcessId
$proc = Get-Process -Id $clippyPid
$cores = [Environment]::ProcessorCount

# --- La tecla de guardar clip ------------------------------------------------------------------------------------------
$cfg = Get-Content -Raw -Encoding UTF8 (Join-Path $Datos "config.json") | ConvertFrom-Json
$vk = [byte]$cfg.hotkey_vk
$mods = [int]$cfg.hotkey_mods
Add-Type -Namespace Medir -Name Teclas -MemberDefinition '[DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);'
# MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN -> sus teclas.
$modKeys = @(@(1, 0x12), @(2, 0x11), @(4, 0x10), @(8, 0x5B)) | Where-Object { $mods -band $_[0] } | ForEach-Object { [byte]$_[1] }
function Press-SaveKey {
    foreach ($k in $modKeys) { [Medir.Teclas]::keybd_event($k, 0, 0, [UIntPtr]::Zero) }
    [Medir.Teclas]::keybd_event($vk, 0, 0, [UIntPtr]::Zero)
    [Medir.Teclas]::keybd_event($vk, 0, 2, [UIntPtr]::Zero)
    foreach ($k in $modKeys) { [Medir.Teclas]::keybd_event($k, 0, 2, [UIntPtr]::Zero) }
}

# --- Una muestra: CPU y RAM del proceso, uso de cada motor de la placa y memoria de video ------------------------------
# La placa sale de los contadores de Windows en crudo (sus nombres en WMI no cambian con el idioma de Windows); el uso es
# la diferencia entre dos muestras, como lo calcula el Administrador de tareas.
$prevCpu = $proc.TotalProcessorTime
$prevAt = [Diagnostics.Stopwatch]::StartNew()
$prevEngines = @{}
$selfProc = Get-Process -Id $PID
$selfCpu0 = $selfProc.TotalProcessorTime

function Take-Sample {
    $proc.Refresh()
    $elapsed = $script:prevAt.Elapsed.TotalSeconds
    $script:prevAt.Restart()
    $cpu = $proc.TotalProcessorTime
    $cpuPct = ($cpu - $script:prevCpu).TotalSeconds / [Math]::Max($elapsed, 0.001) / $cores * 100
    $script:prevCpu = $cpu

    # En WQL "_" es un comodín: entre corchetes, para que el PID 1234 no traiga también el 12345.
    $filter = "Name LIKE 'pid[_]$($clippyPid)[_]%'"
    $engines = @{}
    $use = @{ "3D" = 0.0; "Copy" = 0.0; "VideoEncode" = 0.0; "VideoProcessing" = 0.0; "Otros" = 0.0 }
    foreach ($e in Get-CimInstance Win32_PerfRawData_GPUPerformanceCounters_GPUEngine -Filter $filter -ErrorAction SilentlyContinue) {
        $engines[$e.Name] = @([double]$e.UtilizationPercentage, [double]$e.Timestamp_Sys100NS)
        $prev = $script:prevEngines[$e.Name]
        if (-not $prev) { continue }
        $dt = [double]$e.Timestamp_Sys100NS - $prev[1]
        if ($dt -le 0) { continue }
        $pct = ([double]$e.UtilizationPercentage - $prev[0]) / $dt * 100
        $type = if ($e.Name -match "engtype_(.+)$") { $Matches[1] } else { "Otros" }
        if (-not $use.ContainsKey($type)) { $type = "Otros" }
        $use[$type] += $pct
    }
    $script:prevEngines = $engines
    $vram = 0.0
    foreach ($m in Get-CimInstance Win32_PerfRawData_GPUPerformanceCounters_GPUProcessMemory -Filter $filter -ErrorAction SilentlyContinue) {
        $vram += [double]$m.DedicatedUsage
    }
    $gpuMax = ($use.Values | Measure-Object -Maximum).Maximum
    [pscustomobject]@{
        cpu_pct        = [Math]::Round($cpuPct, 2)
        ram_privada_mb = [Math]::Round($proc.PrivateMemorySize64 / 1MB, 1)
        ram_ws_mb      = [Math]::Round($proc.WorkingSet64 / 1MB, 1)
        vram_mb        = [Math]::Round($vram / 1MB, 1)
        gpu_pct        = [Math]::Round([Math]::Min($gpuMax, 100), 2)
        gpu_3d         = [Math]::Round($use["3D"], 2)
        gpu_copia      = [Math]::Round($use["Copy"], 2)
        gpu_encoder    = [Math]::Round($use["VideoEncode"], 2)
        gpu_conversion = [Math]::Round($use["VideoProcessing"], 2)
        gpu_otros      = [Math]::Round($use["Otros"], 2)
    }
}

# --- clippy.log: lo que escribió Clippy desde cierto punto ------------------------------------------------------------
$logPath = Join-Path $Datos "clippy.log"
function Log-Length { if (Test-Path $logPath) { (Get-Item $logPath).Length } else { 0 } }
function Read-LogFrom([long]$offset) {
    if (-not (Test-Path $logPath)) { return "" }
    $fs = [IO.File]::Open($logPath, "Open", "Read", "ReadWrite")
    try {
        # Clippy borra el registro al pasar de 512 KB: si se achicó, se lee desde el principio.
        if ($fs.Length -lt $offset) { $offset = 0 }
        [void]$fs.Seek($offset, "Begin")
        (New-Object IO.StreamReader($fs, [Text.Encoding]::UTF8)).ReadToEnd()
    } finally { $fs.Dispose() }
}

# --- La prueba ---------------------------------------------------------------------------------------------------------
New-Item -ItemType Directory -Force -Path $Salida | Out-Null
$stamp = Get-Date -Format "yyyy-MM-dd_HH-mm-ss"
$csvPath = Join-Path $Salida "$($Nombre)_$stamp.csv"
$rows = New-Object Collections.Generic.List[object]
$clock = [Diagnostics.Stopwatch]::StartNew()
$saves = New-Object Collections.Generic.List[object]

function Add-Row([string]$phase, [int]$clip) {
    $s = Take-Sample
    $row = [pscustomobject]@{ segundo = [Math]::Round($clock.Elapsed.TotalSeconds, 1); fase = $phase; clip = $clip }
    foreach ($p in $s.PSObject.Properties) { $row | Add-Member -NotePropertyName $p.Name -NotePropertyValue $p.Value }
    $rows.Add($row)
    Write-Host ("{0,6:N1}s {1,-10} CPU {2,5:N1} %  RAM {3,6:N0} MB  VRAM {4,5:N0} MB  GPU {5,5:N1} %" -f $row.segundo, $phase, $row.cpu_pct, $row.ram_privada_mb, $row.vram_mb, $row.gpu_pct)
}

Write-Host "Midiendo Clippy (PID $clippyPid), $Clips clips, en $csvPath"
[void](Take-Sample) # la primera muestra solo arma la base para calcular el uso
Start-Sleep -Seconds 1

for ($clip = 1; $clip -le $Clips; $clip++) {
    for ($i = 0; $i -lt $Antes; $i++) { Add-Row "grabando" $clip; Start-Sleep -Seconds 1 }

    $offset = Log-Length
    $t0 = $clock.Elapsed
    Press-SaveKey
    # Hasta que el registro diga cómo terminó (se mira cada 50 ms, una muestra por segundo mientras tanto).
    $next = $clock.Elapsed.TotalSeconds + 1
    $result = $null
    while (-not $result -and ($clock.Elapsed - $t0).TotalSeconds -lt 60) {
        Start-Sleep -Milliseconds 50
        $text = Read-LogFrom $offset
        if ($text -match "clip guardado: .*") { $result = $Matches[0] }
        elseif ($text -match "no se pudo guardar el clip.*") { $result = $Matches[0] }
        if ($clock.Elapsed.TotalSeconds -ge $next) { Add-Row "guardando" $clip; $next += 1 }
    }
    $ms = [Math]::Round(($clock.Elapsed - $t0).TotalMilliseconds)
    $held = if ($result -match "frenada (\d+) ms") { [int]$Matches[1] } else { $null }
    $mix = if ($result -match "mezcla (\d+) ms") { [int]$Matches[1] } else { $null }
    $mp4 = if ($result -match "MP4 (\d+) ms") { [int]$Matches[1] } else { $null }
    if (-not $result) { $result = "sin respuesta en 60 s (¿Clippy está grabando?)" }
    $saves.Add([pscustomobject]@{ clip = $clip; total_ms = $ms; frenada_ms = $held; mezcla_ms = $mix; mp4_ms = $mp4 })
    Write-Host "  clip $($clip): $ms ms hasta guardarse. $result"

    for ($i = 0; $i -lt $Despues; $i++) { Add-Row "despues" $clip; Start-Sleep -Seconds 1 }
}

$rows | Export-Csv -NoTypeInformation -Encoding UTF8 -Path $csvPath

# --- Resumen: promedio grabando contra el pico al guardar --------------------------------------------------------------
function Stat($items, [string]$field, [string]$how) {
    $vals = @($items | ForEach-Object { $_.$field } | Where-Object { $_ -ne $null })
    if ($vals.Count -eq 0) { return $null }
    $m = $vals | Measure-Object -Average -Maximum
    if ($how -eq "max") { [Math]::Round($m.Maximum, 2) } else { [Math]::Round($m.Average, 2) }
}
$rec = $rows | Where-Object { $_.fase -eq "grabando" }
$save = $rows | Where-Object { $_.fase -ne "grabando" }
$selfProc.Refresh()
$selfCpu = ($selfProc.TotalProcessorTime - $selfCpu0).TotalSeconds / $clock.Elapsed.TotalSeconds / $cores * 100
$summary = [pscustomobject]@{
    nombre                  = $Nombre
    fecha                   = $stamp
    clips                   = $Clips
    cpu_grabando_pct        = Stat $rec cpu_pct avg
    cpu_pico_guardando_pct  = Stat $save cpu_pct max
    ram_grabando_mb         = Stat $rec ram_privada_mb avg
    ram_pico_guardando_mb   = Stat $save ram_privada_mb max
    vram_grabando_mb        = Stat $rec vram_mb avg
    vram_pico_guardando_mb  = Stat $save vram_mb max
    gpu_grabando_pct        = Stat $rec gpu_pct avg
    gpu_pico_guardando_pct  = Stat $save gpu_pct max
    guardado_promedio_ms    = Stat $saves total_ms avg
    grabacion_frenada_ms    = Stat $saves frenada_ms avg
    mezcla_ms               = Stat $saves mezcla_ms avg
    mp4_ms                  = Stat $saves mp4_ms avg
    costo_medicion_cpu_pct  = [Math]::Round($selfCpu, 2)
}
$summaryPath = Join-Path $Salida "resumen.csv"
$summary | Export-Csv -NoTypeInformation -Encoding UTF8 -Append -Path $summaryPath
Write-Host ""
$summary | Format-List
Write-Host "Cada segundo: $csvPath"
Write-Host "Resumen (se suma una fila por corrida, para comparar): $summaryPath"
Write-Host "Los tiempos de Clippy (frenada, mezcla, MP4) solo los escriben las versiones posteriores a la 0.4.6; en las anteriores quedan vacíos."
