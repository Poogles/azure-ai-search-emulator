using System.Diagnostics;

namespace Emulator.Tests.Infrastructure;

/// <summary>
/// Minimal Docker CLI helpers for the image-build step only. The container
/// lifecycle itself is driven by Testcontainers (see
/// <see cref="EmulatorEndpoint"/>), matching the Python harness in
/// source/tests/python/conftest.py, which builds the image with the Docker CLI
/// and runs it through testcontainers.
/// </summary>
internal static class Docker
{
    public const string ImageName = "aisearch-emulator";

    /// <summary>Locate the repository root by walking up from the test working
    /// directory until <c>source/rust/Cargo.toml</c> is found.</summary>
    public static DirectoryInfo FindRepoRoot()
    {
        var dir = new DirectoryInfo(Directory.GetCurrentDirectory());
        while (dir is not null)
        {
            if (File.Exists(Path.Combine(dir.FullName, "source", "rust", "Cargo.toml")))
            {
                return dir;
            }
            dir = dir.Parent;
        }
        throw new InvalidOperationException(
            "could not locate the repository root (source/rust/Cargo.toml) from "
            + Directory.GetCurrentDirectory());
    }

    public static bool ImageExists(string name)
    {
        return Run("image", "inspect", name).ExitCode == 0;
    }

    public static void BuildImage(string name, string context)
    {
        var result = Run("build", "-t", name, context);
        if (result.ExitCode != 0)
        {
            throw new InvalidOperationException(
                $"docker build failed:\n{result.StandardError}");
        }
    }

    private static ProcessResult Run(params string[] args)
    {
        var psi = new ProcessStartInfo("docker")
        {
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            UseShellExecute = false,
        };
        foreach (var arg in args)
        {
            psi.ArgumentList.Add(arg);
        }
        using var process = Process.Start(psi)
            ?? throw new InvalidOperationException("failed to start docker");
        var stdout = process.StandardOutput.ReadToEnd();
        var stderr = process.StandardError.ReadToEnd();
        process.WaitForExit();
        return new ProcessResult(process.ExitCode, stdout, stderr);
    }

    private sealed record ProcessResult(int ExitCode, string StandardOutput, string StandardError);
}
