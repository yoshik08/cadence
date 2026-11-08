<#
.SYNOPSIS
  Samples the memory of an already running Cadence process and checks it against the 100 MB target.

.DESCRIPTION
  Does NOT launch Cadence. Start it yourself (normal run, sign in, play a track, open the library),
  then run this in another terminal. Every -IntervalSec it records:
    private working set  (the target metric; what Task Manager calls "Memory (active private working set)")
    commit               (PrivateMemorySize64, bytes the process has committed)
    working set          (total, includes shared DLL / font pages; for reference only)
    threads and handles
  and finishes with min / avg / max / final and PASS or FAIL against -LimitMB (judged on the maximum).
  Ctrl+C stops early and still prints the summary. Exit code: 0 pass, 1 fail, 2 process not found.
  1 MB here is 1 MiB (1048576 bytes), the unit Task Manager uses.

.EXAMPLE
  powershell -File scripts\measure.ps1 -Minutes 10 -IntervalSec 30
#>
param(
    [double]$Minutes = 10,
    [int]$IntervalSec = 30,
    [string]$Name = 'cadence',
    [double]$LimitMB = 100
)

$ErrorActionPreference = 'Stop'
if ($IntervalSec -lt 1) { $IntervalSec = 1 }
$MB = 1MB

$procs = @(Get-Process -Name $Name -ErrorAction SilentlyContinue)
if ($procs.Count -eq 0) {
    Write-Host "No process named '$Name' is running. Start Cadence first (this script does not launch it)."
    exit 2
}
if ($procs.Count -gt 1) {
    Write-Host ("Several '{0}' processes found (PIDs {1}); measuring the oldest." -f $Name, (($procs | ForEach-Object Id) -join ', '))
}
$proc = $procs | Sort-Object StartTime | Select-Object -First 1
$procId = $proc.Id

# Private working set by PID. The CIM class is not localised and does not depend on the
# "cadence" / "cadence#1" counter instance naming; Get-Counter is the fallback.
$source = 'cim'
function Get-PrivateWS([int]$ProcId) {
    if ($script:source -eq 'cim') {
        try {
            $r = Get-CimInstance Win32_PerfFormattedData_PerfProc_Process -Filter "IDProcess=$ProcId"
            if ($r) { return [double]$r.WorkingSetPrivate }
        } catch { }
        $script:source = 'counter'
    }
    if ($script:source -eq 'counter') {
        try {
            $ids = (Get-Counter '\Process(*)\ID Process' -ErrorAction Stop).CounterSamples |
                Where-Object { $_.CookedValue -eq $ProcId } | Select-Object -First 1
            if ($ids) {
                $inst = $ids.InstanceName
                return [double](Get-Counter "\Process($inst)\Working Set - Private" -ErrorAction Stop).CounterSamples[0].CookedValue
            }
        } catch { }
        $script:source = 'workingset'
    }
    return $null
}

$total = [int][math]::Floor($Minutes * 60 / $IntervalSec) + 1
$rows = New-Object System.Collections.Generic.List[object]
$started = Get-Date

Write-Host ("Cadence PID {0}, {1} samples every {2}s (about {3:N1} min), limit {4} MB private working set" -f $procId, $total, $IntervalSec, $Minutes, $LimitMB)
Write-Host ""
Write-Host ("{0,8} {1,12} {2,10} {3,10} {4,8} {5,8}" -f 'time', 'private WS', 'commit', 'total WS', 'threads', 'handles')

try {
    for ($i = 0; $i -lt $total; $i++) {
        if ($i -gt 0) { Start-Sleep -Seconds $IntervalSec }
        try {
            $proc.Refresh()
            if ($proc.HasExited) { Write-Host "Process exited."; break }
        } catch { Write-Host "Process is gone."; break }

        $priv = Get-PrivateWS $procId
        if ($null -eq $priv) { $priv = [double]$proc.WorkingSet64 }   # last resort, overstates
        $row = [pscustomobject]@{
            T       = [int]((Get-Date) - $started).TotalSeconds
            PrivWS  = $priv / $MB
            Commit  = $proc.PrivateMemorySize64 / $MB
            WS      = $proc.WorkingSet64 / $MB
            Threads = $proc.Threads.Count
            Handles = $proc.HandleCount
        }
        $rows.Add($row)
        Write-Host ("{0,7}s {1,9:N1} MB {2,7:N1} MB {3,7:N1} MB {4,8} {5,8}" -f $row.T, $row.PrivWS, $row.Commit, $row.WS, $row.Threads, $row.Handles)
    }
}
finally {
    Write-Host ""
    if ($rows.Count -eq 0) {
        Write-Host "No samples taken."
    } else {
        $p = $rows | ForEach-Object PrivWS
        $stat = $p | Measure-Object -Minimum -Maximum -Average
        $final = $rows[$rows.Count - 1]
        $peakCommit = ($rows | Measure-Object Commit -Maximum).Maximum
        $peakThreads = ($rows | Measure-Object Threads -Maximum).Maximum
        Write-Host ("Private working set over {0} samples ({1}s): min {2:N1} MB, avg {3:N1} MB, max {4:N1} MB, final {5:N1} MB" -f $rows.Count, $final.T, $stat.Minimum, $stat.Average, $stat.Maximum, $final.PrivWS)
        Write-Host ("Peak commit {0:N1} MB, peak threads {1}, final handles {2}" -f $peakCommit, $peakThreads, $final.Handles)
        if ($source -eq 'workingset') { Write-Host "Note: private working set counters were unavailable; figures are total working set and overstate." }
        if ($stat.Maximum -lt $LimitMB) {
            Write-Host ("PASS: max {0:N1} MB is under the {1} MB target" -f $stat.Maximum, $LimitMB)
            $script:code = 0
        } else {
            Write-Host ("FAIL: max {0:N1} MB is over the {1} MB target" -f $stat.Maximum, $LimitMB)
            $script:code = 1
        }
    }
}
exit $(if ($null -ne $script:code) { $script:code } else { 0 })
