using KotorinToys;
using System.Security.Principal;
using System.Text.Json;
class FakeGateway : IStartupGateway {
 public Dictionary<string,ScheduledTaskState> Tasks = new();
 public bool FailDelete;
 public ScheduledTaskState? Read(string name)=>Tasks.GetValueOrDefault(name);
 public ScheduledTaskState Create(TaskPlan plan)=>throw new NotSupportedException();
 public ScheduledTaskState Restore(string name,string xml,string securityDescriptor=""){var task=new ScheduledTaskState(name,xml,securityDescriptor);Tasks.Add(name,task);return task;}
 public void Delete(string name,string fingerprint){if(FailDelete)throw new IOException("模擬失敗");if(Tasks[name].Fingerprint!=fingerprint)throw new IOException("指紋不一致");Tasks.Remove(name);}
}
static class Program {
 const string Id="voice-mixer-spk-router";
 static int cases;
 static void Check(bool value,string name){if(!value)throw new Exception(name);cases++;Console.WriteLine("成功: "+name);}
 static int Main(string[] args){
  var root=Path.GetFullPath(args[0]);Directory.CreateDirectory(root);
  var sid=WindowsIdentity.GetCurrent().User!.Value;
  (HostSettingsStore,FakeGateway,HostSettings,string) Setup(){
   var store=new HostSettingsStore(Path.Combine(root,Guid.NewGuid().ToString("N")));var gateway=new FakeGateway();
   var name="KotorinToys."+sid+"."+Id;var token=Guid.NewGuid().ToString("N");
   var xml=$"""<Task xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task"><RegistrationInfo><Description>KotorinToys:{token}:{Id}</Description></RegistrationInfo></Task>""";
   var task=new ScheduledTaskState(name,xml);gateway.Tasks[name]=task;
   var before=new HostSettings{OwnerToken=token,HostStartup=true,StartupModes=new(){{Id,"direct_task"},{"other-tool","via_kotorintoys"}},OwnedTasks=new(){{Id,new(name,task.Fingerprint,xml)}}};
   store.Save(before);return(store,gateway,before,name);
  }
  {var(s,g,b,n)=Setup();var removal=new RemovalReservations(s,g);Check(removal.Remove(Id).Success,"対象の所有予約を解除");
   Check(s.Load().StartupModes[Id]=="none" && s.Load().StartupModes["other-tool"]=="via_kotorintoys" && s.Load().HostStartup,"他製品と共有ホストを保持");
   Check(!g.Tasks.ContainsKey(n),"対象タスクを削除");Check(removal.Recover(true,Id).Success,"取消時に復元");
   Check(JsonSerializer.Serialize(s.Load(),ToolCatalog.Json)==JsonSerializer.Serialize(b,ToolCatalog.Json)&&g.Tasks.ContainsKey(n),"予約と管理設定の一致");}
  {var(s,g,b,n)=Setup();g.Tasks[n]=new(n,"<Task/>");Check(!new RemovalReservations(s,g).Remove(Id).Success,"外部変更された予約の削除を拒否");Check(s.Load().StartupModes[Id]=="direct_task","拒否時は設定不変");}
  {var(s,g,b,n)=Setup();g.FailDelete=true;Check(!new RemovalReservations(s,g).Remove(Id).Success,"解除失敗を成功としない");Check(g.Tasks.ContainsKey(n)&&s.Load().StartupModes[Id]=="direct_task","解除失敗で元の設定を保持");}
  {var(s,g,b,n)=Setup();var removal=new RemovalReservations(s,g);Check(!removal.Remove("other-tool").Success,"別製品の指定を拒否");
   Check(!removal.Recover(true,"other-tool").Success || g.Tasks.ContainsKey(n),"別製品への復元で予約を失わない");}
  Console.WriteLine(cases+"件成功。実タスク・実設定には未接触。");return 0;
 }
}
