package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HexFormat;
import java.util.List;
import java.util.regex.Pattern;
import org.jed.storage.*;

/** Shared format-v47 codec, greedy packing and mutation histories. */
public final class GinPostingRunsTest {
    private static final HexFormat HEX=HexFormat.of();
    public static void main(String[]args)throws Exception {
        String text=Files.readString(Path.of(args.length==0?"spec/encoding/gin_posting_runs.toml":args[0]));int count=0;
        for(String section:text.split("(?m)(?=^\\[\\[(?:run|pack|history)]]$)")) {
            if(!section.startsWith("[["))continue;count++;
            byte[]term=HEX.parseHex(field(section,"term"));
            if(section.startsWith("[[run]]")) {
                var keys=bytes(array(section,"keys"));byte[]run=HEX.parseHex(field(section,"run"));
                check(Arrays.equals(GinPostingRuns.encode(term,keys),run),"run encoding");checkKeys(GinPostingRuns.decode(run,term),keys,"run decoding");
            } else {
                PersistentTree tree=PersistentTree.empty(1024,TableLeafCodec.index());
                if(section.startsWith("[[pack]]"))for(byte[]key:bytes(array(section,"keys")))tree=GinPostingRuns.insert(tree,term,key);
                else for(String op:array(section,"ops")){byte[]key=HEX.parseHex(op.substring(1));tree=op.charAt(0)=='+'?GinPostingRuns.insert(tree,term,key):GinPostingRuns.remove(tree,term,key);}
                List<byte[]>actual=new ArrayList<>();try(var cursor=tree.cursor()){PersistentTree.Entry entry;while((entry=cursor.next())!=null)actual.add(entry.key());}
                var expected=bytes(array(section,"runs"));checkKeys(actual,expected,"packed/history runs");
                List<byte[]>expectedKeys=new ArrayList<>();for(byte[]run:expected)expectedKeys.addAll(GinPostingRuns.decode(run,term));
                List<byte[]>actualKeys=new ArrayList<>();try(var cursor=new GinPostingRuns.Cursor(tree,term)){byte[]key;while((key=cursor.next())!=null)actualKeys.add(key);}
                checkKeys(actualKeys,expectedKeys,"posting cursor");
            }
        }
        for(String malformed:new String[]{"","0000","010003","0101000002","010080000002","010000000002"}){
            try{GinPostingRuns.decode(HEX.parseHex(malformed),new byte[0]);throw new AssertionError("malformed run accepted: "+malformed);}catch(CorruptStorageException expected){}
        }
        System.out.println("GinPostingRunsTest: passed "+count+" shared codec/build/mutation vectors and malformed runs");
    }
    private static String field(String section,String name){var match=Pattern.compile("(?m)^"+name+" = \\\"([^\\\"]*)\\\"").matcher(section);if(!match.find())throw new AssertionError("missing "+name);return match.group(1);}
    private static List<String>array(String section,String name){var match=Pattern.compile("(?m)^"+name+" = \\[(.*)]$").matcher(section);if(!match.find())throw new AssertionError("missing "+name);var values=Pattern.compile("\\\"([^\\\"]*)\\\"").matcher(match.group(1));List<String>result=new ArrayList<>();while(values.find())result.add(values.group(1));return result;}
    private static List<byte[]>bytes(List<String>hex){return hex.stream().map(HEX::parseHex).toList();}
    private static void checkKeys(List<byte[]>actual,List<byte[]>expected,String message){check(actual.size()==expected.size(),message+" count");for(int i=0;i<actual.size();i++)check(Arrays.equals(actual.get(i),expected.get(i)),message+" entry "+i);}
    private static void check(boolean condition,String message){if(!condition)throw new AssertionError(message);}
}
