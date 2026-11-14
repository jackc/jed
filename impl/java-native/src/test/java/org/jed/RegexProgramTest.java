package org.jed;

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.nio.charset.StandardCharsets;
import java.util.HexFormat;
import java.util.List;

/** Shared NFA instruction/capture/cost fixtures; these internal facts are not SQL-visible. */
public final class RegexProgramTest {
    private RegexProgramTest() {}
    public static void main(String[] args) throws Exception {
        List<String> rows;
        try (var input = RegexProgramTest.class.getResourceAsStream("/regex.tsv")) { if (input == null) throw new AssertionError("missing regex fixtures"); rows = new BufferedReader(new InputStreamReader(input, StandardCharsets.UTF_8)).lines().toList(); }
        int count = 0;
        try (Database database = Database.memory(); Session session = database.session()) {
            for (String row : rows) {
                if (row.startsWith("#") || row.isEmpty()) continue; String[] fields = row.split("\t", -1); String pattern = decode(fields[1]); RegexProgram program = RegexProgram.compile(pattern,fields[2],false);
                if (fields[0].equals("program")) {
                    equal(decode(fields[4]),String.join(";",program.listing()),"program " + pattern); equal(decode(fields[5]),String.join(";",program.classListing()),"classes " + pattern); equal(fields[6],Integer.toString(program.size()),"program count");
                } else {
                    CostMeter meter = new CostMeter(session,null); RegexProgram.Match match = program.search(decode(fields[3]),0,meter); equal(fields[5],Boolean.toString(match != null),"matched " + pattern); equal(fields[6],Long.toString(meter.cost()),"steps " + pattern);
                    if (match != null) { String[] captures = fields[4].split(","); for (int i = 0; i < captures.length; i++) equal(captures[i],Integer.toString(i % 2 == 0 ? match.start(i / 2) : match.end(i / 2)),"capture " + pattern + " slot " + i); }
                }
                count++;
            }
        }
        try (var input = RegexProgramTest.class.getResourceAsStream("/similar.tsv"); Database database = Database.memory(); Session session = database.session()) {
            if (input == null) throw new AssertionError("missing SIMILAR fixtures");
            for (String row : new BufferedReader(new InputStreamReader(input,StandardCharsets.UTF_8)).lines().toList()) {
                if (row.startsWith("#") || row.isEmpty()) continue; String[] f = row.split("\\t",-1); SimilarFunctions.Translation translation;
                try { translation = SimilarFunctions.translate(decode(f[0]),decode(f[1])); if (!f[11].isEmpty()) throw new AssertionError("expected translation error " + f[11]); }
                catch (SqlException failure) { equal(f[11],failure.sqlState(),"translation error"); count++; continue; }
                equal(decode(f[3]),translation.text(),"SIMILAR translation"); RegexProgram program = RegexProgram.compile(translation.text(),"s",false); equal(f[5],Integer.toString(program.size()),"SIMILAR compile"); CostMeter meter = new CostMeter(session,null); equal(f[4],Boolean.toString(program.search(decode(f[2]),0,meter) != null),"SIMILAR match"); equal(f[6],Long.toString(meter.cost()),"SIMILAR steps");
                try { var programs = new SimilarFunctions.SlicePrograms(translation); if (!f[12].isEmpty()) throw new AssertionError("expected substring error " + f[12]); equal(f[9],Integer.toString(programs.size()),"SIMILAR substring compile"); meter = new CostMeter(session,null); String result = programs.search(decode(f[2]),meter); equal(f[10],Long.toString(meter.cost()),"SIMILAR substring steps"); equal(f[8],Boolean.toString(result == null),"SIMILAR substring null"); if (result != null) equal(decode(f[7]),result,"SIMILAR substring"); }
                catch (SqlException failure) { equal(f[12],failure.sqlState(),"substring error"); }
                count++;
            }
        }
        System.out.println("RegexProgramTest passed (" + count + " shared program/capture/cost vectors)");
    }
    private static String decode(String hex) { return new String(HexFormat.of().parseHex(hex),StandardCharsets.UTF_8); }
    private static void equal(String expected,String actual,String what) { if (!expected.equals(actual)) throw new AssertionError(what + ": expected " + expected + ", got " + actual); }
}
