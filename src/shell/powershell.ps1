[Console]::OutputEncoding = [Text.UTF8Encoding]::new($false)
$OutputEncoding = [Console]::OutputEncoding
# PowerShell may prepend its own installation directory on startup. Restore
# the originating user's PATH once, then allow normal session-local mutations.
if (Test-Path Env:LOCALSHELLD_INITIAL_PATH) {
    $env:PATH = $env:LOCALSHELLD_INITIAL_PATH
    Remove-Item Env:LOCALSHELLD_INITIAL_PATH
}
# Command stdin and the protocol reader must never share their read cursor.
Add-Type -TypeDefinition @'
using System;
using System.IO;
using System.Collections.Concurrent;
using System.Globalization;
using System.Management.Automation;
using System.Management.Automation.Host;
using System.Management.Automation.Runspaces;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
public sealed class LocalshelldInput : IDisposable {
    [DllImport("kernel32.dll", SetLastError=true)] static extern IntPtr GetStdHandle(int kind);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool SetStdHandle(int kind, IntPtr handle);
    [DllImport("kernel32.dll", SetLastError=true)] static extern bool SetHandleInformation(IntPtr handle, uint mask, uint flags);
    readonly IntPtr previous;
    readonly TextReader reader;
    FileStream file;
    public LocalshelldInput(byte[] bytes) {
        previous = GetStdHandle(-10);
        reader = Console.In;
        string path = Path.Combine(Path.GetTempPath(), "localshelld-" + Guid.NewGuid().ToString("N") + ".stdin");
        file = new FileStream(path, FileMode.CreateNew, FileAccess.ReadWrite, FileShare.Read | FileShare.Delete, 4096, FileOptions.DeleteOnClose);
        try {
            file.Write(bytes, 0, bytes.Length);
            file.Flush(); file.Position = 0;
            IntPtr handle = file.SafeFileHandle.DangerousGetHandle();
            if (!SetHandleInformation(handle, 1, 1) || !SetStdHandle(-10, handle))
                throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            Console.SetIn(new StreamReader(file, new UTF8Encoding(false), true, 4096, true));
        } catch { Dispose(); throw; }
    }
    public void Dispose() {
        SetStdHandle(-10, previous);
        Console.SetIn(reader);
        if (file != null) { file.Dispose(); file = null; }
    }
}
// The command pipeline has its own reusable runspace. Stopping one pipeline
// leaves its variables, functions and location intact for the next command.
public sealed class LocalshelldHost : PSHost {
    readonly PSHost parent;
    public LocalshelldHost(PSHost parent) { this.parent = parent; }
    public override Guid InstanceId { get; } = Guid.NewGuid();
    public override string Name => "trustd persistent shell";
    public override Version Version => new Version(1, 0);
    public override PSHostUserInterface UI => parent.UI;
    public override CultureInfo CurrentCulture => parent.CurrentCulture;
    public override CultureInfo CurrentUICulture => parent.CurrentUICulture;
    public override void SetShouldExit(int exitCode) { Environment.Exit(exitCode); }
    public override void EnterNestedPrompt() { throw new NotSupportedException("Interactive prompts are unavailable"); }
    public override void ExitNestedPrompt() { throw new NotSupportedException("Interactive prompts are unavailable"); }
    public override void NotifyBeginApplication() { }
    public override void NotifyEndApplication() { }

    public static void Run(PSHost host, string location, string wrapper, PSObject profile) {
        using var lines = new BlockingCollection<string>(16);
        var reader = Console.In;
        var thread = new Thread(() => {
            try {
                string line;
                while ((line = reader.ReadLine()) != null) lines.Add(line);
            } finally { lines.CompleteAdding(); }
        });
        thread.IsBackground = true;
        thread.Start();
        using var runspace = RunspaceFactory.CreateRunspace(new LocalshelldHost(host));
        runspace.Open();
        runspace.SessionStateProxy.SetVariable("PROFILE", profile);
        using (var startup = PowerShell.Create()) {
            startup.Runspace = runspace;
            startup.AddScript(@"
foreach ($path in @($PROFILE.AllUsersAllHosts, $PROFILE.AllUsersCurrentHost, $PROFILE.CurrentUserAllHosts, $PROFILE.CurrentUserCurrentHost)) {
    if ($path -and (Test-Path -LiteralPath $path -PathType Leaf)) { . $path }
}
", false);
            foreach (var output in startup.Invoke()) Console.Out.WriteLine(output.ToString());
            foreach (var error in startup.Streams.Error) Console.Error.WriteLine(error.ToString());
        }
        runspace.SessionStateProxy.Path.SetLocation(location);
        foreach (var line in lines.GetConsumingEnumerable()) {
            // A late interruption must never affect the following command.
            if (line.StartsWith("!", StringComparison.Ordinal)) continue;
            var parts = line.Split(new[] { '|' }, 3);
            if (parts.Length != 3) continue;
            var marker = parts[0];
            runspace.SessionStateProxy.SetVariable("__localshelld_s", Encoding.UTF8.GetString(Convert.FromBase64String(parts[1])));
            runspace.SessionStateProxy.SetVariable("__localshelld_stdin", Convert.FromBase64String(parts[2]));
            runspace.SessionStateProxy.SetVariable("__localshelld_ec", 130);
            using var pipeline = PowerShell.Create();
            pipeline.Runspace = runspace;
            pipeline.AddScript(wrapper, false);
            var invocation = pipeline.BeginInvoke();
            IAsyncResult stopping = null;
            while (!invocation.IsCompleted) {
                if (lines.TryTake(out var control, 20) && control == "!" + marker && stopping == null)
                    stopping = pipeline.BeginStop(null, null);
            }
            try { pipeline.EndInvoke(invocation); }
            catch (PipelineStoppedException) { }
            if (stopping != null) pipeline.EndStop(stopping);
            var code = stopping == null ? Convert.ToInt32(runspace.SessionStateProxy.GetVariable("__localshelld_ec")) : 130;
            var cwd = Convert.ToBase64String(Encoding.UTF8.GetBytes(runspace.SessionStateProxy.Path.CurrentLocation.Path));
            var frame = "\x1e" + marker + "|" + code + "|" + cwd + "\x1f";
            Console.Out.Write(frame); Console.Out.Flush();
            Console.Error.Write(frame); Console.Error.Flush();
        }
    }
}
'@
$__localshelld_wrapper = @'
    $__localshelld_ec = 0
    $__localshelld_input = $null
    try {
        $__localshelld_input = [LocalshelldInput]::new($__localshelld_stdin)
        $global:LASTEXITCODE = 0
        $__localshelld_had_error = $false
        $__localshelld_errors = $global:Error.Count
        . ([ScriptBlock]::Create($__localshelld_s)) 2>&1 | ForEach-Object {
            if ($_ -is [System.Management.Automation.ErrorRecord]) {
                # Native stderr is wrapped as ErrorRecord by a PowerShell
                # pipeline, but its text alone must not change the exit code.
                if ($_.FullyQualifiedErrorId -notin @('NativeCommandError', 'NativeCommandErrorMessage')) {
                    $__localshelld_had_error = $true
                }
                [Console]::Error.Write(($_ | Out-String -Width 32767))
            } else {
                $_ | Out-String -Stream -Width 32767 | ForEach-Object { [Console]::Out.WriteLine($_) }
            }
        }
        $__localshelld_ec = if ($LASTEXITCODE -ne 0) {
            [int]$LASTEXITCODE
        } elseif ($__localshelld_had_error -or $global:Error.Count -gt $__localshelld_errors) {
            1
        } else {
            0
        }
    } catch {
        $__localshelld_ec = 1
        [Console]::Error.Write(($_ | Out-String -Width 32767))
    } finally {
        if ($null -ne $__localshelld_input) { $__localshelld_input.Dispose() }
    }
'@
[LocalshelldHost]::Run($Host, (Get-Location).Path, $__localshelld_wrapper, [psobject]$PROFILE)
