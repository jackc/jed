package org.jed;

import java.nio.file.Path;

/** Opt-in experiment checks and complete-file conformance runner. */
public final class Check {
    private Check() {}
    public static void main(String[] args) throws Exception {
        if (args.length == 0) throw new IllegalArgumentException("Expected check, query SQL, or corpus FILE...");
        switch (args[0]) {
            case "stress" -> {
                if (args.length != 3) throw new IllegalArgumentException("stress INPUT STORAGE");
                StressRunner.run(args[1], args[2]);
            }
            case "simulation" -> Simulation.main(java.util.Arrays.copyOfRange(args, 1, args.length));
            case "process-actor" -> ProcessActor.main(java.util.Arrays.copyOfRange(args, 1, args.length));
            case "page-service" -> {
                if (args.length != 2) throw new IllegalArgumentException("page-service requires frozen Go actor");
                PageServiceTest.main(new String[0]);
                PageServiceMixedTest.main(new String[] {args[1]});
            }
            case "check" -> {
                if (args.length > 2) throw new IllegalArgumentException("check accepts an optional frozen spec directory");
                SimulationRoleHostTest.main(new String[0]);
                SessionVariablesTest.main(new String[0]);
                HostFunctionsTest.main(new String[0]);
                HostDependenciesTest.main(new String[0]);
                PreparedStatementTest.main(new String[0]);
                PageProtocolTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "pageservice/fixtures.toml").toString() });
                ScratchDeviceTest.main(new String[0]);
                CredentialChangeTest.main(new String[0]);
                EncryptedDatabaseTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/fixtures").toString() });
                RekeyTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/fixtures").toString() });
                CryptoTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat").toString() });
                EncryptedRecoveryTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat").toString() });
                EntropySourcesTest.main(new String[0]);
                RegexProgramTest.main(new String[0]);
                TimeZoneDataTest.main(new String[0]);
                UnicodeDataTest.main(new String[0]);
                CollationHostTest.main(new String[0]);
                IndexDependenciesTest.main(new String[0]);
                ValueFoundationTest.main(new String[0]);
                ExplainExpressionTest.main(new String[0]);
            AggregateEstimateTest.main(new String[0]);
                HostLifecycleTest.main(new String[0]);
                SequenceSessionTest.main(new String[0]);
                IndexStorageTest.main(new String[0]);
                IndexKeyReaderTest.main(new String[0]);
                GistStorageTest.main(new String[0]);
                StorageLimitTest.main(new String[0]);
                AttachmentHostTest.main(new String[0]);
                GinPostingRunsTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "encoding/gin_posting_runs.toml").toString() });
                EncodingTest.main(new String[0]);
                RowSpoolTest.main(new String[0]);
                StoredRowTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/fixtures").toString() });
                TableLeafCodecTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/fixtures").toString() });
                org.jed.storage.BlockStoreTest.main(new String[0]);
                org.jed.storage.PagerTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/fixtures").toString() });
                org.jed.storage.PersistentTreeTest.main(new String[0]);
                org.jed.storage.CatalogChainTest.main(new String[0]);
                org.jed.storage.Lz4Test.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/lz4_vectors.toml").toString() });
                org.jed.storage.OverflowChainTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/fixtures").toString() });
                CatalogCodecTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/fixtures").toString() });
                ImageWriterTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/fixtures").toString() });
                FileDatabaseTest.main(new String[0]);
                org.jed.storage.DurableCommitterTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/fixtures").toString() });
                org.jed.storage.ManifestEncoderTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/fixtures").toString() });
                org.jed.host.LinuxNativeTest.main(new String[0]);
                org.jed.storage.PlaintextRecoveryTest.main(new String[] { Path.of(args.length == 2 ? args[1] : "spec", "fileformat/fixtures").toString() });
                System.out.println("Java foundation host checks passed; full parity remains unverified");
            }
            case "query" -> {
                if (args.length != 2) throw new IllegalArgumentException("query requires one SQL argument");
                try (Database database = Database.memory(); Session session = database.session(); Rows rows = session.query(args[1])) {
                    System.out.println("names=" + rows.names());
                    System.out.println("types=" + rows.types());
                    Row row;
                    while ((row = rows.next()) != null) System.out.println(row.values());
                    System.out.println("cost=" + rows.cost());
                }
            }
            case "corpus", "corpus-file", "corpus-encrypted" -> {
                if (args.length < 2) throw new IllegalArgumentException("corpus requires complete files");
                int failed = 0, records = 0, inapplicable = 0;
                for (int i = 1; i < args.length; i++) {
                    try { int count = Corpus.run(Path.of(args[i]), !args[0].equals("corpus"), args[0].equals("corpus-encrypted")); if (count < 0) inapplicable++; else records += count; }
                    catch (Exception error) {
                        failed++;
                        System.err.println("FAIL " + args[i] + ": " + error.getMessage());
                        if ("1".equals(System.getenv("JED_CONFORMANCE_TRACE"))) error.printStackTrace(System.err);
                    }
                }
                System.out.println("Java corpus: " + (args.length - 1) + " files attempted, " + failed + " failed, " + inapplicable + " inapplicable by shared storage-mode directive, " + records + " records in passing files");
                if (failed != 0) System.exit(1);
            }
            default -> throw new IllegalArgumentException("Unknown command: " + args[0]);
        }
    }
}
