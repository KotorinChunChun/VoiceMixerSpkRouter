using System.Diagnostics;
using System.IO.Pipes;
using System.Security.Principal;
using System.Text.Json;
using Microsoft.Win32;
using KotorinToys;
internal static class Program {
    const string Id="voice-mixer-spk-router", Name="VoiceMixerSpkRouter", AppId="KotorinToys.VoiceMixerSpkRouter";
    static string Argument(string[] args,string name) {
        var i=Array.IndexOf(args,name);if(i<0||i+1>=args.Length)throw new IOException("必須引数がありません："+name);return args[i+1];
    }
    internal static void ValidateRoot(string root) {
        if(!Path.IsPathFullyQualified(root))throw new IOException("絶対パスが必要です。");
        for(var dir=new DirectoryInfo(root);dir!=null;dir=dir.Parent)
            if(dir.Exists && (dir.Attributes&FileAttributes.ReparsePoint)!=0)throw new IOException("リンクを経由した配置は操作しません。");
        using var tool=JsonDocument.Parse(File.ReadAllText(Path.Combine(root,"tool.json")));
        if(tool.RootElement.GetProperty("id").GetString()!=Id||tool.RootElement.GetProperty("appId").GetString()!=AppId)throw new IOException("対象製品が一致しません。");
        var version=FileVersionInfo.GetVersionInfo(Path.Combine(root,Name+".exe"));
        if(version.ProductName!=Name)throw new IOException("実EXEの製品名が一致しません。");
        using var key=Registry.CurrentUser.OpenSubKey(@"Software\Microsoft\Windows\CurrentVersion\Uninstall\"+AppId+"_is1");
        if(key?.GetValue("InstallLocation") is string registered &&
            !string.Equals(Path.TrimEndingDirectorySeparator(Path.GetFullPath(registered)),root,StringComparison.OrdinalIgnoreCase))
            throw new IOException("登録された導入先が異なります。");
    }
    static bool Matches(Process process,string exe) {
        try{return process.Id!=Environment.ProcessId && string.Equals(process.MainModule?.FileName,exe,StringComparison.OrdinalIgnoreCase);}
        catch(System.ComponentModel.Win32Exception){return false;}
        catch(InvalidOperationException){return false;}
    }
    static async Task<JsonElement> Send(string command,int? expectedPid=null) {
        using var deadline=new CancellationTokenSource(TimeSpan.FromSeconds(2));
        var sid=WindowsIdentity.GetCurrent().User!.Value;
        using var pipe=new NamedPipeClientStream(".", $"KotorinToys.{sid}.{Id}", PipeDirection.InOut,PipeOptions.Asynchronous|PipeOptions.CurrentUserOnly);
        await pipe.ConnectAsync(deadline.Token);
        using var writer=new StreamWriter(pipe,leaveOpen:true){AutoFlush=true};
        using var reader=new StreamReader(pipe,leaveOpen:true);
        var requestId=Guid.NewGuid().ToString("N");
        await writer.WriteLineAsync(JsonSerializer.Serialize(new{command,expectedPid,requestId}).AsMemory(),deadline.Token);
        var line=await reader.ReadLineAsync(deadline.Token)??throw new IOException("管理応答がありません。");
        if(line.Length>8192)throw new IOException("管理応答の上限を超えました。");
        using var doc=JsonDocument.Parse(line);var reply=doc.RootElement;
        if(reply.GetProperty("protocolVersion").GetInt32()!=2||!reply.GetProperty("ready").GetBoolean()||
            reply.GetProperty("requestId").GetString()!=requestId)throw new IOException("管理応答が一致しません。");
        return reply.Clone();
    }
    static async Task Stop(string exe) {
        Process? target=null;
        try {
            foreach(var candidate in Process.GetProcesses()) {
                if(Matches(candidate,exe)){if(target!=null){candidate.Dispose();throw new IOException("対象が複数起動しています。");}target=candidate;}
                else candidate.Dispose();
            }
            if(target==null)return;
            var status=await Send("status");
            if(status.GetProperty("pid").GetInt32()!=target.Id||target.SessionId!=Process.GetCurrentProcess().SessionId||!Matches(target,exe))
                throw new IOException("管理対象のPID・セッション・EXEが一致しません。");
            var reply=await Send("stop",target.Id);
            if(reply.GetProperty("pid").GetInt32()!=target.Id || reply.GetProperty("result").GetString()!="accepted")
                throw new IOException("正常終了を拒否しました："+reply.GetProperty("detail").GetString());
            using var deadline=new CancellationTokenSource(TimeSpan.FromSeconds(15));
            await target.WaitForExitAsync(deadline.Token);
        } finally { target?.Dispose(); }
    }
    public static async Task<int> Main(string[] args) {
        try {
            var operation=Argument(args,"--operation");
            if(operation is not ("update" or "remove" or "recover"))throw new IOException("未対応の操作です。");
            if(Argument(args,"--product")!=Name)throw new IOException("未対応の製品です。");
            var root=Path.TrimEndingDirectorySeparator(Path.GetFullPath(Argument(args,"--root")));
            ValidateRoot(root);
            if(operation!="recover")await Stop(Path.Combine(root,Name+".exe"));
            // 未導入の本体の設定は新規作成しない。製品の設定・トークンには触れない。
            var hostDirectory=HostSettingsStore.DefaultDirectory;
            if(File.Exists(Path.Combine(hostDirectory,"settings.json")) && operation!="update") {
                var store=new HostSettingsStore(hostDirectory);
                var settings=store.Load();
                var owned=settings.StartupModes.ContainsKey(Id)||settings.OwnedTasks.ContainsKey(Id);
                var completed=Path.Combine(hostDirectory,"last-removal-operation.json");
                if(operation=="recover" && File.Exists(completed)) {
                    using var journal=JsonDocument.Parse(File.ReadAllText(completed));
                    owned=journal.RootElement.GetProperty("targetId").GetString()==Id;
                }
                if(owned) {
                    var reservations=new RemovalReservations(store,new TaskSchedulerGateway());
                    var result=operation=="remove"?reservations.Remove(Id):reservations.Recover(true,Id);
                    if(!result.Success)throw new IOException(result.Detail);
                }
            }
            Console.WriteLine("対象照合・正常停止完了。設定・履歴は保持します。");return 0;
        } catch(Exception ex) { Console.Error.WriteLine(ex.Message);return 1; }
    }
}
