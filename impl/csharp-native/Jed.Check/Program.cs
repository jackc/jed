using Jed;

try
{
    if (args is ["test", var root])
    {
        await HostTests.Run();
        await ParameterHostTests.Run();
        await DatabaseTests.Run();
        DecimalTests.Run();
        EncodingTests.Run(root);
        TemporalTests.Run(root);
        TimeZoneTests.Run(root);
        await OpenStorageTests.Run(root);
        await StorageTests.Run(root);
        await StorageLargeValuesTests.Run(root);
        await HarnessTests.Run();
        await PageWireTests.Run(root);
        await FileCoordinationTests.Run();
        await PageClientTests.Run();
        await PageOwnerTests.Run();
        await DatabaseFileTests.Run();
        CryptoTests.Run(root);
        ScalarBytesTests.Run();
        FloatTests.Run();
        StorageCryptoTests.Run(root);
        await StorageCryptoTests.Recovery(root);
        await Corpus.Run(Path.Combine(root, "spec/conformance/suites/query/scalar_foundations.test"));
    }
    else if (args is ["service-interop", var peerExecutable])
    {
        await FileInteropTests.Run(peerExecutable);
    }
    else if (args is ["page-service", var serviceRoot])
    {
        await PageWireTests.Run(serviceRoot);
        await FileCoordinationTests.Run();
        await PageClientTests.Run();
        await PageOwnerTests.Run();
        await DatabaseFileTests.Run();
    }
    else if (args is ["encoding", var encodingRoot])
    {
        EncodingTests.Run(encodingRoot);
        TemporalTests.Run(encodingRoot);
        TimeZoneTests.Run(encodingRoot);
        await OpenStorageTests.Run(encodingRoot);
    }
    else if (args is ["crypto", var cryptoRoot])
    {
        CryptoTests.Run(cryptoRoot);
        StorageCryptoTests.Run(cryptoRoot);
        await StorageCryptoTests.Recovery(cryptoRoot);
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
        Console.Error.WriteLine("usage: Jed.Check test <repository> | crypto <repository> | encoding <repository> | page-service <repository> | service-interop <peer-actor> | corpus <file>...");
        return 2;
    }
    return 0;
}
catch (Exception error)
{
    Console.Error.WriteLine(error);
    return 1;
}
