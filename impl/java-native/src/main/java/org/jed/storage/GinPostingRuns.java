package org.jed.storage;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Format-v47 bounded posting runs. Each mutation decodes at most one existing run. */
public final class GinPostingRuns {
    public static final int MAX_BYTES = 64;
    private GinPostingRuns() {}

    public static byte[] encode(byte[] term, List<byte[]> keys) {
        if (keys.isEmpty()) throw new IllegalArgumentException("empty posting run");
        byte[] last = keys.getLast(), previous = last;
        if (last.length == 0) throw new IllegalArgumentException("empty posting key");
        ByteArrayOutputStream body = new ByteArrayOutputStream();
        for (int i = 0; i < keys.size() - 1; i++) {
            byte[] key = keys.get(i);
            if (key.length == 0 || i > 0 && compare(keys.get(i-1),key) >= 0 || compare(key,last) >= 0)
                throw new IllegalArgumentException("unordered posting keys");
            int shared = 0;
            while (shared < Math.min(previous.length,key.length) && previous[shared] == key[shared]) shared++;
            varint(body, shared); varint(body, key.length-shared); body.write(key,shared,key.length-shared); previous=key;
        }
        if (body.size() > 65535) throw new IllegalArgumentException("posting body too large");
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        out.writeBytes(term); out.writeBytes(last); out.writeBytes(body.toByteArray()); out.write(body.size() >>> 8); out.write(body.size());
        return out.toByteArray();
    }

    public static List<byte[]> decode(byte[] run, byte[] term) throws CorruptStorageException {
        if (run.length < term.length + 3 || !startsWith(run,term)) throw corrupt();
        int size = (run[run.length-2]&255)*256 + (run[run.length-1]&255);
        int bodyStart = run.length-2-size;
        if (bodyStart <= term.length) throw corrupt();
        byte[] last = Arrays.copyOfRange(run,term.length,bodyStart), previous=last;
        List<byte[]> keys = new ArrayList<>(); int[] at = {bodyStart};
        while (at[0] < run.length-2) {
            int shared = varint(run,at,run.length-2), suffix = varint(run,at,run.length-2);
            if (shared > previous.length || suffix > run.length-2-at[0] || (long)shared+suffix > Integer.MAX_VALUE || shared+suffix == 0) throw corrupt();
            byte[] key = Arrays.copyOf(previous,shared+suffix);
            System.arraycopy(run,at[0],key,shared,suffix); at[0]+=suffix;
            if (compare(key,last)>=0 || !keys.isEmpty() && compare(keys.getLast(),key)>=0) throw corrupt();
            keys.add(key); previous=key;
        }
        keys.add(last);
        if (keys.size()>1 && run.length>MAX_BYTES) throw corrupt();
        return List.copyOf(keys);
    }

    public static PersistentTree insert(PersistentTree tree, byte[] term, byte[] key) throws IOException {
        if (key.length==0) throw new IllegalArgumentException("empty posting key");
        byte[] search=join(term,key), upper=after(term); PersistentTree.Entry next;
        try(var cursor=tree.cursor(search,true,upper,false)){next=cursor.next();}
        if(next!=null) {
            List<byte[]> keys=new ArrayList<>(decode(next.key(),term)); int position=lowerBound(keys,key);
            if(position<keys.size() && compare(keys.get(position),key)==0)return tree;
            keys.add(position,key.clone()); tree=tree.remove(next.key()); return split(tree,term,keys);
        }
        PersistentTree.Entry previous;
        try(var cursor=tree.reverseCursor(term,true,search,false)){previous=cursor.next();}
        if(previous!=null) {
            List<byte[]> keys=new ArrayList<>(decode(previous.key(),term)); keys.add(key.clone()); byte[] encoded=encode(term,keys);
            if(encoded.length<=MAX_BYTES)return tree.remove(previous.key()).put(encoded,new byte[0]);
        }
        return tree.put(encode(term,List.of(key)),new byte[0]);
    }

    public static PersistentTree remove(PersistentTree tree, byte[] term, byte[] key) throws IOException {
        PersistentTree.Entry next;
        try(var cursor=tree.cursor(join(term,key),true,after(term),false)){next=cursor.next();}
        if(next==null)return tree;
        List<byte[]> keys=new ArrayList<>(decode(next.key(),term)); int position=lowerBound(keys,key);
        if(position==keys.size() || compare(keys.get(position),key)!=0)return tree;
        keys.remove(position); tree=tree.remove(next.key()); return keys.isEmpty()?tree:split(tree,term,keys);
    }
    private static PersistentTree split(PersistentTree tree,byte[]term,List<byte[]>keys)throws IOException {
        byte[] encoded=encode(term,keys);
        if(encoded.length<=MAX_BYTES || keys.size()==1)return tree.put(encoded,new byte[0]);
        int middle=(keys.size()+1)/2;
        tree=split(tree,term,keys.subList(0,middle)); return split(tree,term,keys.subList(middle,keys.size()));
    }

    /** Incremental posting reader; keeps at most one decoded run plus the tree cursor. */
    public static final class Cursor implements AutoCloseable {
        private final PersistentTree.Cursor cursor; private final byte[] term;
        private List<byte[]> run=List.of(); private int at; private byte[] previous;
        public Cursor(PersistentTree tree,byte[]term) { this.term=term.clone();cursor=tree.cursor(term,true,after(term),false); }
        public byte[] next() throws IOException {
            if(at==run.size()) {var entry=cursor.next();if(entry==null)return null;run=decode(entry.key(),term);at=0;}
            byte[] key=run.get(at++);
            if(previous!=null && compare(previous,key)>=0)throw corrupt();previous=key;return key.clone();
        }
        public void close(){cursor.close();run=List.of();at=0;previous=null;}
    }
    private static int compare(byte[]a,byte[]b){return Arrays.compareUnsigned(a,b);}
    private static int lowerBound(List<byte[]>keys,byte[]key){int low=0,high=keys.size();while(low<high){int mid=(low+high)>>>1;if(compare(keys.get(mid),key)<0)low=mid+1;else high=mid;}return low;}
    private static byte[] join(byte[]a,byte[]b){byte[]result=Arrays.copyOf(a,a.length+b.length);System.arraycopy(b,0,result,a.length,b.length);return result;}
    private static boolean startsWith(byte[]bytes,byte[]prefix){return bytes.length>=prefix.length && Arrays.equals(bytes,0,prefix.length,prefix,0,prefix.length);}
    private static byte[] after(byte[]prefix){for(int i=prefix.length-1;i>=0;i--)if((prefix[i]&255)!=255){byte[]result=Arrays.copyOf(prefix,i+1);result[i]++;return result;}return null;}
    private static void varint(ByteArrayOutputStream out,int n){do{int b=n&127;n>>>=7;out.write(b|(n==0?0:128));}while(n!=0);}
    private static int varint(byte[]bytes,int[]at,int end)throws CorruptStorageException{long result=0;for(int shift=0;shift<=28;shift+=7){if(at[0]>=end)throw corrupt();int b=bytes[at[0]++]&255;result|=(long)(b&127)<<shift;if((b&128)==0){if(result>Integer.MAX_VALUE || shift!=0 && b==0)throw corrupt();return (int)result;}}throw corrupt();}
    private static CorruptStorageException corrupt(){return new CorruptStorageException("invalid GIN posting run");}
}
