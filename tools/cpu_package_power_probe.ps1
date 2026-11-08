[CmdletBinding()]
param(
    [int]$Samples = 10,
    [int]$IntervalMs = 1000,
    [string]$ModulePath = "",
    [string]$PawnIoDll = "C:\Program Files\PawnIO\PawnIOLib.dll"
)

$ErrorActionPreference = "Stop"

$principal = New-Object Security.Principal.WindowsPrincipal(
    [Security.Principal.WindowsIdentity]::GetCurrent()
)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this probe from an elevated PowerShell window."
}
if (-not (Test-Path -LiteralPath $PawnIoDll)) {
    throw "PawnIOLib.dll not found: $PawnIoDll"
}
if ([string]::IsNullOrWhiteSpace($ModulePath)) {
    $ModulePath = Join-Path $env:APPDATA "framework-crate\modules\IntelMSR.bin"
}
if (-not (Test-Path -LiteralPath $ModulePath)) {
    throw "IntelMSR.bin not found: $ModulePath (open Framework Crate and download PawnIO Modules first)"
}

$dllLiteral = $PawnIoDll.Replace('\', '\\').Replace('"', '\"')
$source = @"
using System;
using System.Runtime.InteropServices;

public static class PawnIoProbe
{
    [DllImport("$dllLiteral", CallingConvention = CallingConvention.StdCall)]
    public static extern int pawnio_open(out IntPtr handle);

    [DllImport("$dllLiteral", CallingConvention = CallingConvention.StdCall)]
    public static extern int pawnio_load(IntPtr handle, byte[] blob, UIntPtr size);

    [DllImport("$dllLiteral", CallingConvention = CallingConvention.StdCall)]
    public static extern int pawnio_execute(
        IntPtr handle,
        byte[] name,
        ulong[] input,
        UIntPtr inputSize,
        ulong[] output,
        UIntPtr outputSize,
        out UIntPtr returnSize);

    [DllImport("$dllLiteral", CallingConvention = CallingConvention.StdCall)]
    public static extern int pawnio_close(IntPtr handle);
}
"@
Add-Type -TypeDefinition $source

$handle = [IntPtr]::Zero
$hr = [PawnIoProbe]::pawnio_open([ref]$handle)
if ($hr -ne 0 -or $handle -eq [IntPtr]::Zero) {
    throw ("pawnio_open failed: 0x{0:X8}" -f $hr)
}

try {
    $blob = [System.IO.File]::ReadAllBytes($ModulePath)
    $blobSize = [UIntPtr]::new([uint64]$blob.Length)
    $hr = [PawnIoProbe]::pawnio_load($handle, $blob, $blobSize)
    if ($hr -ne 0) {
        throw ("pawnio_load failed: 0x{0:X8}" -f $hr)
    }

    $msrName = [byte[]]([System.Text.Encoding]::ASCII.GetBytes("ioctl_read_msr") + [byte]0)
    $oneSize = [UIntPtr]::new([uint64]1)

    function Read-Msr([IntPtr]$Handle, [uint32]$Index) {
        $inData = [uint64[]]@([uint64]$Index)
        $outData = [uint64[]]::new(1)
        $returnSize = [UIntPtr]::Zero
        $result = [PawnIoProbe]::pawnio_execute(
            $Handle,
            $msrName,
            $inData,
            $oneSize,
            $outData,
            $oneSize,
            [ref]$returnSize
        )
        if ($result -ne 0) {
            throw ("pawnio_execute(MSR 0x{0:X}) failed: 0x{1:X8}" -f $Index, $result)
        }
        if ($returnSize.ToUInt64() -ne 1) {
            throw ("pawnio_execute(MSR 0x{0:X}) returned {1} qwords" -f $Index, $returnSize)
        }
        return [uint64]$outData[0]
    }

    $units = Read-Msr $handle 0x606
    $esu = [int](($units -shr 8) -band 0x1F)
    $energyUnit = 1.0 / [math]::Pow(2.0, $esu)
    Write-Host ("Energy unit: 1/{0} J per count (ESU={1})" -f [math]::Pow(2, $esu), $esu)
    Write-Host "time,raw_energy,delta_counts,package_watts"

    $lastRaw = [uint64](Read-Msr $handle 0x611) -band [uint64]4294967295
    $lastTime = Get-Date

    for ($i = 0; $i -lt $Samples; $i++) {
        Start-Sleep -Milliseconds $IntervalMs
        $nowRaw = [uint64](Read-Msr $handle 0x611) -band [uint64]4294967295
        $nowTime = Get-Date
        $seconds = ($nowTime - $lastTime).TotalSeconds
        if ($seconds -le 0.01) {
            continue
        }

        if ($nowRaw -ge $lastRaw) {
            $delta = $nowRaw - $lastRaw
        } else {
            $delta = ([uint64]4294967295 - $lastRaw) + $nowRaw
        }
        $watts = ([double]$delta * $energyUnit) / $seconds
        Write-Host ("{0:HH:mm:ss.fff},{1},{2},{3:N2}" -f $nowTime, $nowRaw, $delta, $watts)

        $lastRaw = $nowRaw
        $lastTime = $nowTime
    }
}
finally {
    if ($handle -ne [IntPtr]::Zero) {
        [void][PawnIoProbe]::pawnio_close($handle)
    }
}

