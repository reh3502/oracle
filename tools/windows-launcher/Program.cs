using System.Diagnostics;
using System.Security.AccessControl;
using System.Security.Principal;
using System.Text.Json.Nodes;

namespace OracleLauncher;

internal static class Program
{
    [STAThread]
    static int Main(string[] args)
    {
        if (args.Length > 0 && args[0] == "--close-check")
        {
            if (args.Length != 2) return 2;
            ApplicationConfiguration.Initialize();
            var controller = new BotController(Path.GetFullPath(args[1]), true);
            using var form = new LauncherForm(controller, true);
            Application.Run(form);
            bool passed = !controller.Running && form.CheckFailure is null;
            Directory.CreateDirectory(controller.Root);
            File.WriteAllText(Path.Combine(controller.Root, "close-result.txt"),
                passed ? "PASS: actual FormClosing during startup stopped the host\n" : "FAIL: " + (form.CheckFailure ?? "host still running") + "\n");
            return passed ? 0 : 1;
        }
        if (args.Length > 0 && args[0] is "--smoke-test" or "--live-check")
        {
            if (args.Length != 2) return 2;
            // The explicit path keeps verification separate from the real deployment.
            bool offline = args[0] == "--smoke-test";
            var controller = new BotController(Path.GetFullPath(args[1]), offline, !offline);
            string resultFile = Path.Combine(controller.Root, offline ? "smoke-result.txt" : "check-result.txt");
            try
            {
                if (!offline && File.Exists(Path.Combine(controller.Root, "oracle.json")))
                    throw new IOException("Use a fresh isolated directory for live verification.");
                controller.StartAsync().GetAwaiter().GetResult();
                controller.VerifyAsync().GetAwaiter().GetResult();
                controller.StopAsync().GetAwaiter().GetResult();
                controller.StartAsync().GetAwaiter().GetResult();
                controller.VerifyAsync().GetAwaiter().GetResult();
                controller.StopAsync().GetAwaiter().GetResult();
                File.WriteAllText(resultFile, offline
                    ? "PASS: offline install, load, health, stop, restart, health, stop\n"
                    : "PASS: authenticated start, install, activate, command publication, cached lookup, stop, restart, cached lookup, stop\n");
                return 0;
            }
            catch (Exception error)
            {
                try { controller.StopAsync().GetAwaiter().GetResult(); } catch { }
                Directory.CreateDirectory(controller.Root);
                File.WriteAllText(resultFile, "FAIL: " + error.Message + "\n");
                return 1;
            }
        }
        using var instance = new Mutex(true, @"Local\OracleSisterLauncher", out bool owns);
        if (!owns) { MessageBox.Show("Oracle is already open.", "Oracle"); return 0; }
        ApplicationConfiguration.Initialize();
        Application.Run(new LauncherForm());
        return 0;
    }
}

internal sealed class BotController
{
    internal string Root { get; }
    readonly bool smoke;
    readonly bool liveCheck;
    readonly string bundle = AppContext.BaseDirectory;
    Process? host;
    CancellationToken operationCancellation;
    TimeSpan commandTimeout = TimeSpan.FromSeconds(60);
    bool ready;
    Task? outputDrain, errorDrain;
    string Config => Path.Combine(Root, "oracle.json");
    string Guild => JsonNode.Parse(File.ReadAllText(Config))!["guilds"]![0]!["guild"]!.GetValue<string>();
    internal bool Running => host is { HasExited: false };
    internal BotController(string? root = null, bool smoke = false, bool liveCheck = false)
    {
        Root = root ?? Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "OracleSister");
        this.smoke = smoke;
        this.liveCheck = liveCheck;
    }

    static void PrivateDirectory(string path)
    {
        if (Directory.Exists(path) && (File.GetAttributes(path) & FileAttributes.ReparsePoint) != 0)
            throw new IOException("The data directory cannot be a link.");
        var user = WindowsIdentity.GetCurrent().User ?? throw new IOException("Cannot identify Windows account.");
        var acl = new DirectorySecurity();
        acl.SetOwner(user);
        acl.SetAccessRuleProtection(true, false);
        foreach (var sid in new[] { user, new SecurityIdentifier(WellKnownSidType.LocalSystemSid, null) })
            acl.AddAccessRule(new FileSystemAccessRule(sid, FileSystemRights.FullControl,
                InheritanceFlags.ContainerInherit | InheritanceFlags.ObjectInherit,
                PropagationFlags.None, AccessControlType.Allow));
        var directory = new DirectoryInfo(path);
        if (!directory.Exists) directory.Create(acl);
        else
        {
            var existing = directory.GetAccessControl();
            if (existing.GetOwner(typeof(SecurityIdentifier))?.Equals(user) != true ||
                existing.GetAccessRules(true, true, typeof(SecurityIdentifier)).Cast<FileSystemAccessRule>().Any(rule =>
                    rule.AccessControlType == AccessControlType.Allow &&
                    !rule.IdentityReference.Equals(user) &&
                    !rule.IdentityReference.Equals(new SecurityIdentifier(WellKnownSidType.LocalSystemSid, null))))
                throw new IOException("The existing Oracle data directory is not private. Keep it for recovery and choose a new private deployment.");
        }
    }
    static FileStream PrivateFile(string path)
    {
        if (File.Exists(path) && (File.GetAttributes(path) & FileAttributes.ReparsePoint) != 0)
            throw new IOException("Private files cannot be links.");
        var user = WindowsIdentity.GetCurrent().User ?? throw new IOException("Cannot identify Windows account.");
        var acl = new FileSecurity();
        acl.SetOwner(user); acl.SetAccessRuleProtection(true, false);
        foreach (var sid in new[] { user, new SecurityIdentifier(WellKnownSidType.LocalSystemSid, null) })
            acl.AddAccessRule(new FileSystemAccessRule(sid, FileSystemRights.FullControl, AccessControlType.Allow));
        return new FileInfo(path).Create(FileMode.Create, FileSystemRights.FullControl,
            FileShare.None, 4096, FileOptions.None, acl);
    }
    static void CopyTree(string source, string target)
    {
        if ((File.GetAttributes(source) & FileAttributes.ReparsePoint) != 0)
            throw new IOException("Package links are unsupported.");
        PrivateDirectory(target);
        foreach (string file in Directory.GetFiles(source))
        {
            if ((File.GetAttributes(file) & FileAttributes.ReparsePoint) != 0)
                throw new IOException("Package links are unsupported.");
            string destination = Path.Combine(target, Path.GetFileName(file));
            using var sourceFile = File.OpenRead(file);
            using var destinationFile = PrivateFile(destination);
            sourceFile.CopyTo(destinationFile);
            destinationFile.Flush(true);
        }
        foreach (string directory in Directory.GetDirectories(source))
            CopyTree(directory, Path.Combine(target, Path.GetFileName(directory)));
    }
    void Prepare()
    {
        PrivateDirectory(Root);
        if (!File.Exists(Config))
        {
            string initializing = Path.Combine(Root, ".bootstrap-in-progress");
            if (Directory.Exists(Path.Combine(Root, "state")) ||
                (Directory.EnumerateFileSystemEntries(Root).Any() && !File.Exists(initializing)))
                throw new IOException("Oracle data already exists without its configuration. Keep it for recovery; setup will not overwrite it.");
            using (var marker = PrivateFile(initializing)) { marker.WriteByte(1); marker.Flush(true); }
            CopyTree(Path.Combine(bundle, "payload", "catalog"), Path.Combine(Root, "catalog"));
            CopyTree(Path.Combine(bundle, "payload", "module-package"), Path.Combine(Root, "module-package"));
            CopyTree(Path.Combine(bundle, "payload", "python"), Path.Combine(Root, "python"));
            CopyTree(Path.Combine(bundle, "payload", "refresh-worker"), Path.Combine(Root, "refresh-worker"));
            var refresh = new JsonObject {
                ["enabled"] = !smoke, ["source_access_qualified"] = !smoke,
                ["python"] = Path.Combine(Root, "python", "python.exe"),
                ["worker"] = Path.Combine(Root, "refresh-worker", "refresh_worker.py")
            };
            string refreshPath = Path.Combine(Root, "catalog", "refresh-settings.json");
            using (var stream = PrivateFile(refreshPath))
            {
                stream.Write(System.Text.Encoding.UTF8.GetBytes(refresh.ToJsonString()));
                stream.Flush(true);
            }
            var config = JsonNode.Parse(File.ReadAllText(Path.Combine(bundle, "payload", "oracle.json")))!.AsObject();
            config["state_dir"] = Path.Combine(Root, "state");
            config["database"] = new JsonObject { ["backend"] = "sqlite", ["path"] = Path.Combine(Root, "state", "oracle.sqlite") };
            config["module_runtime"]!["community.dandys-world"]!["data_directory"] = Path.Combine(Root, "catalog");
            config["ai"] = null;
            if (smoke) config["discord"] = null;
            using (var stream = PrivateFile(Config + ".tmp"))
            {
                stream.Write(System.Text.Encoding.UTF8.GetBytes(config.ToJsonString(new() { WriteIndented = true })));
                stream.Flush(true);
            }
            File.Move(Config + ".tmp", Config);
            File.Delete(initializing);
        }
        var saved = JsonNode.Parse(File.ReadAllText(Config))!;
        if (saved["guilds"] is not JsonArray guilds || guilds.Count != 1)
            throw new IOException("This launcher requires exactly one configured server.");
        if (saved["ai"] is not null) throw new IOException("AI must remain disabled for this release.");
        if (smoke && saved["discord"] is not null) throw new IOException("Smoke tests require offline configuration.");
    }
    ProcessStartInfo Command(IEnumerable<string> args)
    {
        var info = new ProcessStartInfo(Path.Combine(bundle, "oracle-host.exe"))
        {
            UseShellExecute = false, CreateNoWindow = true, WorkingDirectory = Root,
            RedirectStandardOutput = true, RedirectStandardError = true
        };
        info.Environment.Remove("DISCORD_TOKEN");
        info.Environment.Remove("GEMINI_API_KEY");
        info.ArgumentList.Add("--config"); info.ArgumentList.Add(Config);
        foreach (string arg in args) info.ArgumentList.Add(arg);
        return info;
    }
    static async Task DrainAsync(StreamReader reader)
    {
        // Host output is intentionally discarded: never surface credentials or gateway data.
        char[] buffer = new char[4096];
        while (await reader.ReadAsync(buffer) != 0) { }
    }
    static async Task<string> ReadControlError(StreamReader reader)
    {
        char[] buffer = new char[4096];
        int used = 0;
        while (used < buffer.Length)
        {
            int count = await reader.ReadAsync(buffer.AsMemory(used));
            if (count == 0) break;
            used += count;
        }
        await DrainAsync(reader);
        var match = System.Text.RegularExpressions.Regex.Match(new string(buffer, 0, used), @"(?:code:\s*|Error:\s*)([A-Za-z]+)");
        return match.Success ? match.Groups[1].Value : "unavailable";
    }
    async Task<string> Cli(params string[] args)
    {
        using var process = Process.Start(Command(args)) ?? throw new IOException("Cannot start bot control.");
        var output = process.StandardOutput.ReadToEndAsync();
        var error = ReadControlError(process.StandardError);
        try { await process.WaitForExitAsync(operationCancellation).WaitAsync(commandTimeout, operationCancellation); }
        catch (OperationCanceledException) { try { process.Kill(true); } catch { } throw; }
        catch { try { process.Kill(true); } catch { } throw new IOException("Bot control timed out."); }
        string errorCode = await error;
        string text = await output;
        if (process.ExitCode != 0) throw new IOException($"Bot control failed: {args[0]}." + (smoke ? " Code: " + errorCode : ""));
        return text;
    }
    internal async Task StartAsync(CancellationToken cancellation = default)
    {
        if (Running) return;
        operationCancellation = cancellation;
        ready = false;
        Prepare();
        cancellation.ThrowIfCancellationRequested();
        string? token = null;
        if (!smoke)
        {
            var file = new FileInfo(Path.Combine(bundle, ".env"));
            if (!file.Exists || file.Length > 4096) throw new IOException("Set DISCORD_TOKEN in .env beside Start Oracle.exe.");
            token = CredentialParser.Parse(await File.ReadAllTextAsync(file.FullName));
        }
        var info = Command(["serve", "--defer-command-publication"]);
        if (token is not null) info.Environment["DISCORD_TOKEN"] = token;
        host = Process.Start(info) ?? throw new IOException("Cannot start the bot.");
        outputDrain = DrainAsync(host.StandardOutput); errorDrain = DrainAsync(host.StandardError);
        try
        {
            var deadline = DateTime.UtcNow.AddSeconds(60);
            while (true)
            {
                if (host.HasExited) throw new IOException("The bot stopped during startup. Check her token and server access.");
                cancellation.ThrowIfCancellationRequested();
                try { await Cli("module", "health"); ready = true; break; }
                catch (Exception error) when (error is not OperationCanceledException && DateTime.UtcNow < deadline) { await Task.Delay(500, cancellation); }
            }
            string marker = Path.Combine(Root, smoke ? "module-loaded-offline" : "module-ready");
            if (!File.Exists(marker))
            {
                var installed = JsonNode.Parse(await Cli("module", "install", "--source", Path.Combine(Root, "module-package"), "--trust-native"))!;
                string digest = installed["digest"]!.GetValue<string>();
                var health = JsonNode.Parse(await Cli("module", "health"))!.AsObject();
                if (!health.ContainsKey("community.dandys-world"))
                    await Cli("module", "load", "--digest", digest);
                if (!smoke)
                {
                var activate = new List<string> { "module", "activate", "--module", "community.dandys-world", "--guild", Guild };
                var manifest = JsonNode.Parse(File.ReadAllText(Path.Combine(Root, "module-package", "package.json")))!;
                foreach (var capability in manifest["manifest"]!["capabilities"]!.AsArray()) { activate.Add("--grant"); activate.Add(capability!.GetValue<string>()); }
                await Cli(activate.ToArray());
                }
                File.WriteAllText(marker, digest);
            }
            if (!smoke)
            {
                // Closing must not abandon a potentially in-flight Discord write.
                // Complete this bounded publication before honoring startup cancel.
                cancellation.ThrowIfCancellationRequested();
                operationCancellation = CancellationToken.None;
                try
                {
                    await Cli("publish-commands");
                    File.WriteAllText(Path.Combine(Root, "commands-ready"), "1");
                }
                finally { operationCancellation = cancellation; }
                cancellation.ThrowIfCancellationRequested();
            }
            await VerifyAsync();
        }
        catch { await StopAsync(); throw; }
    }
    internal async Task VerifyAsync()
    {
        if (smoke)
        {
            var health = JsonNode.Parse(await Cli("module", "health"))?["community.dandys-world"];
            if (health?["global"]?["lifecycle"]?.GetValue<string>() != "Accepting" ||
                health?["generation"]?.GetValue<long>() is not > 0)
                throw new IOException("Offline module did not reach an accepting generation.");
            return;
        }
        var result = JsonNode.Parse(await Cli("module", "invoke", "--module", "community.dandys-world", "--guild", Guild, "--operation", "health"));
        if (result?["reply"]?["text"]?.GetValue<string>().Contains("Loaded wiki snapshot:", StringComparison.Ordinal) != true)
            throw new IOException("The bundled wiki catalog did not load.");
        if (liveCheck)
        {
            string headsText = File.ReadAllText(Path.Combine(Root, "catalog", "active")).Trim();
            string digest = headsText.Length == 64 ? headsText : JsonNode.Parse(headsText)!["current"]!.GetValue<string>();
            if (digest.Length != 64 || digest.Any(c => !char.IsAsciiHexDigit(c))) throw new IOException("Invalid catalog pointer.");
            var catalog = JsonNode.Parse(File.ReadAllText(Path.Combine(Root, "catalog", digest + ".json")))!;
            string name = catalog["entities"]![0]!["name"]!.GetValue<string>();
            var lookup = JsonNode.Parse(await Cli("module", "invoke", "--module", "community.dandys-world", "--guild", Guild,
                "--operation", "lookup", "--input", new JsonObject { ["name"] = name }.ToJsonString()));
            if (string.IsNullOrWhiteSpace(lookup?["reply"]?["text"]?.GetValue<string>()))
                throw new IOException("Cached lookup returned no answer.");
        }
    }
    internal async Task StopAsync()
    {
        operationCancellation = CancellationToken.None;
        if (host is null) return;
        commandTimeout = TimeSpan.FromSeconds(3);
        try
        {
            var deadline = DateTime.UtcNow.AddSeconds(40);
            while (!host.HasExited)
            {
                try { await Cli("stop"); break; }
                catch when (!host.HasExited && DateTime.UtcNow < deadline) { await Task.Delay(250); }
                catch when (host.HasExited) { break; }
                catch when (!ready)
                {
                    // This process never became a running bot. Reap only our own
                    // failed startup tree after allowing control startup to finish.
                    host.Kill(true);
                    break;
                }
            }
            if (!host.HasExited)
                await host.WaitForExitAsync().WaitAsync(TimeSpan.FromSeconds(45));
            if (outputDrain is not null) await outputDrain;
            if (errorDrain is not null) await errorDrain;
            host.Dispose(); host = null;
        }
        finally { commandTimeout = TimeSpan.FromSeconds(60); }
    }

}

internal sealed class LauncherForm : Form
{
    readonly BotController bot;
    readonly bool closeCheck;
    internal string? CheckFailure { get; private set; }
    readonly Label status = new() { Text = "Stopped", AutoSize = true, Top = 35, Left = 25 };
    readonly Button start = new() { Text = "Start bot", Top = 85, Left = 25, Width = 145 };
    readonly Button stop = new() { Text = "Stop bot", Top = 85, Left = 185, Width = 145, Enabled = false };
    bool busy, closing, closeRequested;
    CancellationTokenSource? startup;
    readonly System.Windows.Forms.Timer monitor = new() { Interval = 1000 };
    internal LauncherForm(BotController? controller = null, bool closeCheck = false)
    {
        bot = controller ?? new BotController();
        this.closeCheck = closeCheck;
        if (closeCheck)
        {
            var closeTimer = new System.Windows.Forms.Timer { Interval = 50 };
            closeTimer.Tick += (_, _) => { closeTimer.Stop(); closeTimer.Dispose(); Close(); };
            Shown += (_, _) => closeTimer.Start();
        }
        Text = "Oracle — Dandy's World"; ClientSize = new Size(440, 170);
        FormBorderStyle = FormBorderStyle.FixedDialog; MaximizeBox = false;
        StartPosition = FormStartPosition.CenterScreen;
        Controls.AddRange([status, start, stop]);
        start.Click += async (_, _) => await RunAction(true);
        stop.Click += async (_, _) => await RunAction(false);
        Shown += async (_, _) => await RunAction(true);
        monitor.Tick += (_, _) => { if (!busy && !bot.Running) { status.Text = "Stopped"; start.Enabled = true; stop.Enabled = false; } };
        monitor.Start();
        FormClosing += async (_, e) =>
        {
            if (closing) return;
            e.Cancel = true;
            if (busy)
            {
                closeRequested = true;
                status.Text = "Stopping safely…";
                startup?.Cancel();
                return;
            }
            await RunAction(false);
            if (!bot.Running) { closing = true; monitor.Dispose(); Close(); }
        };
    }
    async Task RunAction(bool run)
    {
        if (busy) return;
        busy = true; start.Enabled = stop.Enabled = false;
        status.Text = run ? "Starting…" : "Stopping safely…";
        try
        {
            if (run) { startup = new(); await bot.StartAsync(startup.Token); }
            else await bot.StopAsync();
            status.Text = bot.Running ? "Running — AI disabled" : "Stopped";
        }
        catch (OperationCanceledException) when (closeRequested) { status.Text = "Stopped"; }
        catch (Exception error)
        {
            status.Text = bot.Running ? "Stop failed — bot is still running" : "Could not start";
            if (closeCheck) CheckFailure = error.Message;
            else MessageBox.Show(this, error.Message, "Oracle", MessageBoxButtons.OK, MessageBoxIcon.Error);
        }
        finally
        {
            startup?.Dispose(); startup = null;
            busy = false; start.Enabled = !bot.Running; stop.Enabled = bot.Running;
            if (closeRequested) { closeRequested = false; BeginInvoke(Close); }
        }
    }
}
