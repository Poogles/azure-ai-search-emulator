namespace Emulator.Tests.Infrastructure;

/// <summary>
/// All emulator-backed test classes share one collection so they run serially
/// against a single container (xunit parallelises across collections, not
/// within one). State isolation between tests is provided by the per-test
/// <c>POST /admin/reset</c> in <see cref="EmulatorTestBase"/>.
/// </summary>
[CollectionDefinition(EmulatorCollection.Name)]
public sealed class EmulatorCollection : ICollectionFixture<EmulatorEndpoint>
{
    public const string Name = "Emulator";
}
