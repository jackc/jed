using Jed;

try
{
    if (args is ["test", var root])
    {
        await HostTests.Run();
        await HarnessTests.Run();
        await Corpus.Run(Path.Combine(root, "spec/conformance/suites/query/scalar_foundations.test"));
    }
    else if (args.Length > 1 && args[0] == "corpus")
    {
        var failures = 0; var records = 0;
        foreach (var file in args.Skip(1))
        {
            try { records += await Corpus.Run(file); }
            catch (Exception error) { Console.Error.WriteLine(error.Message); failures++; }
        }
        Console.WriteLine($"{args.Length - 1} selected files, {records} records in passing files, {failures} failed files; no full-parity claim");
        if (failures > 0) return 1;
    }
    else
    {
        Console.Error.WriteLine("usage: Jed.Check test <repository> | corpus <file>...");
        return 2;
    }
    return 0;
}
catch (Exception error)
{
    Console.Error.WriteLine(error);
    return 1;
}
