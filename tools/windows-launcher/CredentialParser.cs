namespace OracleLauncher;
internal static class CredentialParser
{
    internal static string Parse(string text)
    {
        string? token = null;
        foreach (string raw in text.Split('\n'))
        {
            string line = raw.Trim();
            if (line.Length == 0 || line.StartsWith('#')) continue;
            int separator = line.IndexOf('=');
            if (separator < 1 || line[..separator].Trim() != "DISCORD_TOKEN" || token is not null)
                throw new System.IO.IOException(".env must contain one DISCORD_TOKEN assignment and optional comments.");
            string value = line[(separator + 1)..].Trim();
            if (value.Length >= 2 && ((value[0] == '"' && value[^1] == '"') || (value[0] == '\'' && value[^1] == '\'')))
                value = value[1..^1];
            if (value.Length == 0 || value.Any(c => char.IsWhiteSpace(c) || c is '\'' or '"' or '$' or '`' or '#' or '='))
                throw new System.IO.IOException("DISCORD_TOKEN must be a single bot token; interpolation is unsupported.");
            token = value;
        }
        return token ?? throw new System.IO.IOException("Add DISCORD_TOKEN to .env before starting.");
    }
}
