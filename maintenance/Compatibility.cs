using System.Text.Json;
namespace KotorinToys;
internal sealed record ProductIdentity(string Id) {
    public static ProductIdentity? FromId(string id) => id == "voice-mixer-spk-router" ? new(id) : null;
}
internal static class ToolCatalog {
    public static readonly JsonSerializerOptions Json = new() { PropertyNamingPolicy=JsonNamingPolicy.CamelCase, PropertyNameCaseInsensitive=true, WriteIndented=true };
}
internal sealed record StartupResult(bool Success,string Detail);
internal sealed record ExternalStartupState(string Kind,string Name,string Detail);
