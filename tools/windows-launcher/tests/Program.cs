using OracleLauncher;
foreach (string value in new[] { "DISCORD_TOKEN=abc.def.xyz", "# Sister bot\r\nDISCORD_TOKEN = 'abc.def.xyz'\r\n", "DISCORD_TOKEN=\"abc.def.xyz\"\n" })
    if (CredentialParser.Parse(value) != "abc.def.xyz") throw new Exception("Valid assignment failed");
foreach (string value in new[] { "", "# nothing", "DISCORD_TOKEN=", "DISCORD_TOKEN='abc", "DISCORD_TOKEN=a b", "DISCORD_TOKEN=$SECRET", "DISCORD_TOKEN=a\nDISCORD_TOKEN=b", "OTHER=x\nDISCORD_TOKEN=abc", "export DISCORD_TOKEN=abc", "DISCORD_TOKEN=abc #comment", "DISCORD_TOKEN=`cmd`", "DISCORD_TOKEN=x=y" })
{
    try { CredentialParser.Parse(value); throw new Exception("Invalid assignment accepted"); }
    catch (IOException error) { if (error.Message.Contains(value) && value.Length > 15) throw new Exception("Input leaked in error"); }
}
Console.WriteLine("PASS: literal dotenv parsing, malformed/duplicate/interpolated assignment rejection");
