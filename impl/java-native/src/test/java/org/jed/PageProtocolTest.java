package org.jed;

import java.nio.file.Files;
import java.nio.file.Path;
import java.util.*;
import org.jed.host.PageProtocol;
import static org.jed.host.PageProtocol.*;

/** Shared wire fixtures, including independent field-to-message encoding and malformed bounds. */
public final class PageProtocolTest {
    private static final HexFormat HEX=HexFormat.of();
    public static void main(String[] args)throws Exception {
        List<Map<String,String>> fixtures=new ArrayList<>();Map<String,String> current=null;
        for(String raw:Files.readAllLines(Path.of(args.length == 0 ? "spec/pageservice/fixtures.toml" : args[0]))){String line=raw.trim();
            if(line.startsWith("[[")){current=new HashMap<>();current.put("kind",line.substring(2,line.length()-2));fixtures.add(current);}
            else if(current!=null&&line.contains(" = ")){int at=line.indexOf(" = ");String value=line.substring(at+3);if(value.startsWith("\""))current.put(line.substring(0,at),unquote(value));}}
        int frames=0,owners=0,malformed=0;
        for(var f:fixtures){byte[] expected=HEX.parseHex(f.get("hex"));String kind=f.get("kind");int ps=Integer.parseInt(f.getOrDefault("page_size","0"));
            if(kind.equals("frame")){Frame source=new Frame(number(f,"request_id"),message(f,ps));byte[] actual=encode(source,ps);check(Arrays.equals(actual,expected),f.get("name")+" encode");Frame decoded=decode(expected,ps);check(Arrays.equals(encode(decoded,ps),actual),f.get("name")+" decode");frames++;}
            else if(kind.equals("owner_record")){OwnerRecord source=new OwnerRecord((int)number(f,"f_flags"),number(f,"f_uid"),HEX.parseHex(f.get("f_incarnation")),number(f,"f_pid"),f.get("f_endpoint"));check(Arrays.equals(encodeOwner(source),expected),"owner encode");check(Arrays.equals(encodeOwner(decodeOwner(expected)),expected),"owner decode");owners++;}
            else {try{decode(expected,ps);throw new AssertionError(f.get("name")+" malformed accepted");}catch(Violation good){}malformed++;}}
        // A hostile length is rejected after four bytes, without asking the stream for a body.
        var prefix=new java.io.ByteArrayInputStream(new byte[]{1,0,0,1}){public synchronized int read(byte[] b,int off,int len){if(available()==0)throw new AssertionError("read hostile body");return super.read(b,off,len);}};
        try{read(prefix,256);throw new AssertionError("oversize stream accepted");}catch(Violation good){}
        // Wire strings may contain NUL, but neither malformed UTF-8 nor unpaired host surrogates.
        var nul=new Frame(1,new Hello(1,1,53,0,"a\0b",WireCredential.none()));check(((Hello)decode(encode(nul,0),0).message()).client().equals("a\0b"),"NUL wire string");
        try{encode(new Frame(1,new Hello(1,1,53,0,"\ud800",WireCredential.none())),0);throw new AssertionError("surrogate accepted");}catch(Violation good){}
        System.out.println("PageProtocolTest: passed "+frames+" shared frames, "+owners+" owner records, "+malformed+" malformed frames and stream allocation bounds");
    }
    private static Message message(Map<String,String> f,int ps){return switch(f.get("message")){
        case "hello"->new Hello(integer(f,"protocol_min"),integer(f,"protocol_max"),integer(f,"format_version"),integer(f,"flags"),field(f,"client_name"),credential(field(f,"credential")));
        case "hello_ok"->new HelloOk(integer(f,"protocol"),integer(f,"format_version"),integer(f,"page_size"),n(f,"connection_id"),integer(f,"flags"),integer(f,"max_read_pages"));
        case "begin_read"->new BeginRead(id(f),n(f,"last_txid"));
        case "read_begun"->new ReadBegun(id(f),n(f,"txid"),integer(f,"invalidate"),indices(field(f,"indices")),page(field(f,"meta"),ps));
        case "read_pages"->new ReadPages(n(f,"txid"),indices(field(f,"indices")));
        case "pages"->new Pages(pages(field(f,"pages"),ps));case "end_read"->new EndRead(n(f,"txid"));
        case "begin_write"->new BeginWrite(id(f),n(f,"last_txid"),n(f,"lock_timeout_ms"),integer(f,"flags"));
        case "write_begun"->new WriteBegun(id(f),n(f,"txid"),integer(f,"invalidate"),indices(field(f,"indices")),Boolean.parseBoolean(field(f,"reuse_grant")),page(field(f,"meta"),ps),digests(field(f,"pending")),indices(field(f,"reuse")));
        case "commit_begin"->new CommitBegin(n(f,"txid"),page(field(f,"meta"),ps),indexed(field(f,"manifest_pages"),ps));
        case "commit_pages"->new CommitPages(indexed(field(f,"pages"),ps));case "commit_end"->new CommitEnd();case "abort"->new Abort();
        case "committed"->new Committed(n(f,"txid"));case "commit_status"->new CommitStatus(n(f,"txid"),n(f,"digest"));
        case "commit_status_reply"->new CommitStatusReply(integer(f,"status"));case "request_ownership"->new RequestOwnership();case "ownership_reply"->new OwnershipReply(integer(f,"status"));
        case "notice"->new Notice(integer(f,"kind"),n(f,"connection_id"));case "error"->new PageProtocol.Error(field(f,"sqlstate"),field(f,"message"),field(f,"detail"),integer(f,"flags"));default->throw new AssertionError("unknown fixture");};}
    private static String field(Map<String,String> f,String key){return f.get("f_"+key);}
    private static long number(Map<String,String> f,String key){return Long.parseUnsignedLong(f.get(key));}
    private static long n(Map<String,String> f,String key){return number(f,"f_"+key);}
    private static int integer(Map<String,String> f,String key){return(int)n(f,key);}
    private static byte[] id(Map<String,String> f){return HEX.parseHex(field(f,"incarnation"));}
    private static long[] indices(String value){return value.isEmpty()?new long[0]:Arrays.stream(value.split(",")).mapToLong(Long::parseUnsignedLong).toArray();}
    private static byte[] page(String value,int size){String[] parts=value.split("/",-1);byte[] result=new byte[size];Arrays.fill(result,HEX.parseHex(parts[1])[0]);byte[] prefix=HEX.parseHex(parts[0]);System.arraycopy(prefix,0,result,0,prefix.length);return result;}
    private static List<byte[]> pages(String value,int size){return value.isEmpty()?List.of():Arrays.stream(value.split(";",-1)).map(v->page(v,size)).toList();}
    private static List<IndexedPage> indexed(String value,int size){return value.isEmpty()?List.of():Arrays.stream(value.split(";",-1)).map(v->{String[] p=v.split("=",2);return new IndexedPage(Long.parseUnsignedLong(p[0]),page(p[1],size));}).toList();}
    private static List<Digest> digests(String value){return value.isEmpty()?List.of():Arrays.stream(value.split(",",-1)).map(v->{String[] p=v.split("=",2);return new Digest(Long.parseUnsignedLong(p[0]),Long.parseUnsignedLong(p[1]));}).toList();}
    private static WireCredential credential(String value){return value.isEmpty()?WireCredential.none():value.startsWith("key:")?new WireCredential(2,"",HEX.parseHex(value.substring(4))):new WireCredential(1,value.substring(11),new byte[0]);}
    private static String unquote(String s){s=s.substring(1,s.length()-1);StringBuilder out=new StringBuilder();for(int i=0;i<s.length();i++){char c=s.charAt(i);if(c=='\\'){c=s.charAt(++i);c=switch(c){case 'n'->'\n';case 'r'->'\r';case 't'->'\t';default->c;};}out.append(c);}return out.toString();}
    private static void check(boolean value,String message){if(!value)throw new AssertionError(message);}
}
