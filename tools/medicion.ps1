<#
Lo que miden medir.ps1 y versus.ps1 de un proceso, desde afuera: procesador, RAM, uso de cada motor de la placa, memoria
de video, handles e hilos. Se carga con ". $PSScriptRoot\medicion.ps1".
#>

# La placa sale de los contadores de Windows en crudo (sus nombres en WMI no cambian con el idioma de Windows); el uso es
# la diferencia entre dos muestras, como lo calcula el Administrador de tareas.
function New-Medidor([int]$Id) {
    $proc = Get-Process -Id $Id
    $m = @{
        Id      = $Id
        Proc    = $proc
        Cores   = [Environment]::ProcessorCount
        PrevCpu = $proc.TotalProcessorTime
        At      = [Diagnostics.Stopwatch]::StartNew()
        Engines = @{}
    }
    [void](Read-Medidor $m) # la primera muestra solo arma la base para calcular el uso
    $m
}

function Read-Medidor($m) {
    $proc = $m.Proc
    $proc.Refresh()
    $elapsed = $m.At.Elapsed.TotalSeconds
    $m.At.Restart()
    $cpu = $proc.TotalProcessorTime
    $cpuPct = ($cpu - $m.PrevCpu).TotalSeconds / [Math]::Max($elapsed, 0.001) / $m.Cores * 100
    $m.PrevCpu = $cpu

    # En WQL "_" es un comodín: entre corchetes, para que el PID 1234 no traiga también el 12345.
    $filter = "Name LIKE 'pid[_]$($m.Id)[_]%'"
    $engines = @{}
    $use = @{ "3D" = 0.0; "Copy" = 0.0; "VideoEncode" = 0.0; "VideoProcessing" = 0.0; "Otros" = 0.0 }
    foreach ($e in Get-CimInstance Win32_PerfRawData_GPUPerformanceCounters_GPUEngine -Filter $filter -ErrorAction SilentlyContinue) {
        $engines[$e.Name] = @([double]$e.UtilizationPercentage, [double]$e.Timestamp_Sys100NS)
        $prev = $m.Engines[$e.Name]
        if (-not $prev) { continue }
        $dt = [double]$e.Timestamp_Sys100NS - $prev[1]
        if ($dt -le 0) { continue }
        $pct = ([double]$e.UtilizationPercentage - $prev[0]) / $dt * 100
        $type = if ($e.Name -match "engtype_(.+)$") { $Matches[1] } else { "Otros" }
        if (-not $use.ContainsKey($type)) { $type = "Otros" }
        $use[$type] += $pct
    }
    $m.Engines = $engines
    $vram = 0.0
    foreach ($g in Get-CimInstance Win32_PerfRawData_GPUPerformanceCounters_GPUProcessMemory -Filter $filter -ErrorAction SilentlyContinue) {
        $vram += [double]$g.DedicatedUsage
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
        handles        = $proc.HandleCount
        hilos          = $proc.Threads.Count
    }
}

# Aprieta una tecla con sus modificadores (MOD_ALT = 1, MOD_CONTROL = 2, MOD_SHIFT = 4, MOD_WIN = 8), como el usuario.
Add-Type -Namespace Medicion -Name Teclas -MemberDefinition '[DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);'
function Send-Tecla([int]$Vk, [int]$Mods = 0) {
    $modKeys = @(@(1, 0x12), @(2, 0x11), @(4, 0x10), @(8, 0x5B)) | Where-Object { $Mods -band $_[0] } | ForEach-Object { [byte]$_[1] }
    foreach ($k in $modKeys) { [Medicion.Teclas]::keybd_event($k, 0, 0, [UIntPtr]::Zero) }
    [Medicion.Teclas]::keybd_event([byte]$Vk, 0, 0, [UIntPtr]::Zero)
    [Medicion.Teclas]::keybd_event([byte]$Vk, 0, 2, [UIntPtr]::Zero)
    foreach ($k in $modKeys) { [Medicion.Teclas]::keybd_event($k, 0, 2, [UIntPtr]::Zero) }
}

# Promedio, máximo o percentil 95 de un campo; $null si no hay valores.
function Get-Stat($items, [string]$field, [string]$how = "avg") {
    $vals = @($items | ForEach-Object { $_.$field } | Where-Object { $_ -ne $null } | ForEach-Object { [double]$_ })
    if ($vals.Count -eq 0) { return $null }
    switch ($how) {
        "max" { [Math]::Round(($vals | Measure-Object -Maximum).Maximum, 2) }
        "min" { [Math]::Round(($vals | Measure-Object -Minimum).Minimum, 2) }
        "p95" { $s = $vals | Sort-Object; [Math]::Round($s[[Math]::Min($s.Count - 1, [int][Math]::Floor($s.Count * 0.95))], 2) }
        default { [Math]::Round(($vals | Measure-Object -Average).Average, 2) }
    }
}
