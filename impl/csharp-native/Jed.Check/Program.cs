using Jed;

try
{
    if (args.Length >= 3 && args[0] == "process-actor") return await ProcessActor.Run(args.Skip(1).ToArray());
    if (args is ["test", var root])
    {
        await HostTests.Run();
        await SessionCapabilityTests.Run();
        await ParameterHostTests.Run();
        await DatabaseTests.Run();
        DecimalTests.Run();
        EncodingTests.Run(root);
        await MemoryTests.Run(root);
        await StatisticsTests.Run(root);
        await GinTests.Run(root);
        await GistTests.Run(root);
        CollationTests.Run(root);
        await CollationTests.Host(root);
        RegexTests.Run(root);
        TemporalTests.Run(root);
        TimeZoneTests.Run(root);
        await IndexDependencyTests.Run(root);
        await OpenStorageTests.Run(root);
        await StorageTests.Run(root);
        await StorageLargeValuesTests.Run(root);
        await HarnessTests.Run();
        await PageWireTests.Run(root);
        await FileCoordinationTests.Run();
        await PageClientTests.Run();
        await PageOwnerTests.Run();
        await DatabaseFileTests.Run();
        await CredentialChangeTests.Run(root);
        await RekeyTests.Run(root);
        await DatabaseImageTests.Run(root);
        await DatabaseCompactTests.Run(root);
        await DatabaseBackupTests.Run(root);
        await DatabaseDomainCommitTests.Run(root);
        await SimulationHostTests.Run();
        await StorageShapeTests.Run(root);
        await ScratchTests.Run();
        await SpillOperatorTests.Run();
        await SqlSpillTests.Run();
        CryptoTests.Run(root);
        ScalarBytesTests.Run();
        FloatTests.Run();
        await UuidTests.Run(root);
        StorageCryptoTests.Run(root);
        await StorageCryptoTests.Recovery(root);
        await Corpus.Run(Path.Combine(root, "spec/conformance/suites/query/scalar_foundations.test"));
    }
    else if (args is ["service-interop", var peerExecutable])
    {
        await FileInteropTests.Run(peerExecutable);
    }
    else if (args is ["spill-operators"]) { await SpillOperatorTests.Run(); await SqlSpillTests.Run(); }
    else if (args is ["simulation", var simulationPath]) await SimulationRunner.Run(simulationPath);
    else if (args is ["simulation-host"]) await SimulationHostTests.Run();
    else if (args is ["rekey", var rekeyRoot]) await RekeyTests.Run(rekeyRoot);
    else if (args is ["maintenance", var maintenanceRoot])
    {
        await CredentialChangeTests.Run(maintenanceRoot);
        await RekeyTests.Run(maintenanceRoot);
        await DatabaseImageTests.Run(maintenanceRoot);
        await DatabaseCompactTests.Run(maintenanceRoot);
        await DatabaseBackupTests.Run(maintenanceRoot);
        await DatabaseDomainCommitTests.Run(maintenanceRoot);
        await StorageShapeTests.Run(maintenanceRoot);
        await ScratchTests.Run();
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
        await MemoryTests.Run(encodingRoot);
        await StatisticsTests.Run(encodingRoot);
        await GinTests.Run(encodingRoot);
        await GistTests.Run(encodingRoot);
        CollationTests.Run(encodingRoot);
        RegexTests.Run(encodingRoot);
        TemporalTests.Run(encodingRoot);
        TimeZoneTests.Run(encodingRoot);
        await IndexDependencyTests.Run(encodingRoot);
        await OpenStorageTests.Run(encodingRoot);
    }
    else if (args is ["crypto", var cryptoRoot])
    {
        CryptoTests.Run(cryptoRoot);
        StorageCryptoTests.Run(cryptoRoot);
        await StorageCryptoTests.Recovery(cryptoRoot);
    }
    else if (args.Length > 1 && args[0] is "corpus" or "corpus-file")
    {
        var failures = 0; var records = 0;
        foreach (var file in args.Skip(1))
        {
            try { records += await Corpus.Run(file, args[0] == "corpus-file"); }
            catch (Exception error) { Console.Error.WriteLine(error.Message); failures++; }
        }
        Console.WriteLine($"{args.Length - 1} selected files, {records} records in passing files, {failures} failed files; no full-parity claim");
        if (failures > 0) return 1;
    }
    else
    {
        Console.Error.WriteLine("usage: Jed.Check test <repository> | crypto <repository> | encoding <repository> | maintenance <repository> | page-service <repository> | service-interop <peer-actor> | corpus <file>...");
        return 2;
    }
    return 0;
}
catch (Exception error)
{
    Console.Error.WriteLine(error);
    return 1;
}
