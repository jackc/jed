package org.jed.host;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.StandardOpenOption;
import java.util.HashSet;
import java.util.HexFormat;
import java.util.Set;
import java.util.function.Consumer;

/** Stable protocol-v2 bundle. Claims survive client/owner transitions; only presence authorizes file I/O. */
public final class ExclusiveFileLock implements AutoCloseable {
    public static final class BusyException extends IOException {
        private static final long serialVersionUID = 1L;
        BusyException(String message) { super(message); }
    }
    public static final class ProtocolException extends IOException {
        private static final long serialVersionUID = 1L;
        ProtocolException(String message) { super(message); }
    }
    private static final Set<Path> LOCAL = new HashSet<>();
    public static final class Claim implements AutoCloseable {
        private final Path path,bundle;
        private final long openerPid=ProcessHandle.current().pid();
        private ExclusiveFileLock owner;
        private boolean closed;
        private Claim(Path path){this.path=path;bundle=Path.of(path+".lock");}
        public Path path(){return path;} public Path bundle(){return bundle;}
        public synchronized void checkProcess()throws IOException{if(closed)throw new IOException("file claim closed");if(openerPid!=ProcessHandle.current().pid())throw new IOException("inherited database handle requires reopen");}
        /** A failed election opens no database bytes and leaves no coordination descriptor in a client. */
        public synchronized ExclusiveFileLock tryOwn(Consumer<byte[]> entropy)throws IOException{
            checkProcess();if(owner!=null)throw new IllegalStateException("claim already owns file");
            LinuxNative.LockFile presence=null;boolean success=false;
            try{
                initializeBundle(bundle);presence=LinuxNative.openLockFile(bundle.resolve("presence"));
                try(var arrival=LinuxNative.openLockFile(bundle.resolve("arrival"))){
                    if(!arrival.tryShared())return null;
                    try{validateMarker(bundle);if(!presence.tryExclusive())return null;}finally{arrival.unlock();}
                }
                if(Files.exists(path,LinkOption.NOFOLLOW_LINKS))rejectHardLinks(path);
                byte[] incarnation=new byte[16];int attempts=0;
                do{entropy.accept(incarnation);if(++attempts>1024)throw new IOException("host entropy did not produce an owner incarnation");}while(allZero(incarnation));
                ExclusiveFileLock acquired=new ExclusiveFileLock(this,presence,incarnation);
                acquired.publishEndpoint("");owner=acquired;success=true;return acquired;
            }finally{if(!success&&presence!=null)presence.close();}
        }
        public PageProtocol.OwnerRecord ownerRecord()throws IOException{
            checkProcess();Path record=bundle.resolve("owner");
            try{if(!Files.isRegularFile(record,LinkOption.NOFOLLOW_LINKS)||Files.size(record)>4096)return null;return PageProtocol.decodeOwner(Files.readAllBytes(record));}
            catch(java.nio.file.NoSuchFileException|PageProtocol.Violation absent){return null;}
        }
        public synchronized void close()throws IOException{
            if(closed)return;closed=true;
            try{if(owner!=null)owner.releaseOwnership();}finally{synchronized(LOCAL){LOCAL.remove(path);}}
        }
    }
    private final Claim claim;
    private final LinuxNative.LockFile presence;
    private final byte[] incarnation;
    private boolean released;
    private ExclusiveFileLock(Claim claim,LinuxNative.LockFile presence,byte[] incarnation){this.claim=claim;this.presence=presence;this.incarnation=incarnation;}
    public Path path(){return claim.path();}public Path bundle(){return claim.bundle();}public Claim claim(){return claim;}public byte[] incarnation(){return incarnation.clone();}
    public static long uid()throws IOException{return((Number)Files.getAttribute(Path.of("/proc/self"),"unix:uid")).longValue();}
    public static Claim claim(Path requested,boolean create)throws IOException{
        Path absolute=requested.toAbsolutePath().normalize();Path path=create?absolute.getParent().toRealPath().resolve(absolute.getFileName()):absolute.toRealPath();
        synchronized(LOCAL){if(!LOCAL.add(path))throw new BusyException("database already open in this process");}
        try{if(!create)rejectHardLinks(path);return new Claim(path);}catch(IOException|RuntimeException|java.lang.Error failure){synchronized(LOCAL){LOCAL.remove(path);}throw failure;}
    }
    public static ExclusiveFileLock acquire(Path requested,boolean create)throws IOException{
        Claim claim=claim(requested,create);boolean success=false;
        try{var owner=claim.tryOwn(new java.security.SecureRandom()::nextBytes);if(owner==null)throw new BusyException("database is owned by another process");success=true;return owner;}
        finally{if(!success)claim.close();}
    }
    private static void initializeBundle(Path bundle)throws IOException{
        try{Files.createDirectory(bundle);}catch(java.nio.file.FileAlreadyExistsException exists){}
        if(!Files.isDirectory(bundle,LinkOption.NOFOLLOW_LINKS))throw new IOException("lock bundle is not a real directory");
        try{Files.createFile(bundle.resolve("protocol-v2"));}catch(java.nio.file.FileAlreadyExistsException exists){}
    }
    private static void validateMarker(Path bundle)throws IOException{
        Path marker=bundle.resolve("protocol-v2");if(!Files.isRegularFile(marker,LinkOption.NOFOLLOW_LINKS)||Files.size(marker)!=0)throw new ProtocolException("invalid protocol-v2 marker");
        try(var entries=Files.newDirectoryStream(bundle,"protocol-v*")){for(Path entry:entries){String name=entry.getFileName().toString();if(!name.equals("protocol-v1")&&!name.equals("protocol-v2"))throw new ProtocolException("unsupported database lock protocol: "+name);}}
    }
    private static void rejectHardLinks(Path path)throws IOException{Number links=(Number)Files.getAttribute(path,"unix:nlink",LinkOption.NOFOLLOW_LINKS);if(links.longValue()!=1)throw new BusyException("hard-linked database files cannot share a path lock bundle safely");}
    private static boolean allZero(byte[] bytes){int any=0;for(byte b:bytes)any|=b;return any==0;}
    public synchronized void publishEndpoint(String endpoint)throws IOException{
        if(released)throw new IOException("ownership released");
        byte[] bytes=PageProtocol.encodeOwner(new PageProtocol.OwnerRecord(endpoint.isEmpty()?0:1,uid(),incarnation,ProcessHandle.current().pid(),endpoint));
        Path temporary=bundle().resolve("owner.tmp-"+HexFormat.of().formatHex(incarnation));
        try{Files.write(temporary,bytes,StandardOpenOption.CREATE_NEW,StandardOpenOption.WRITE);Files.move(temporary,bundle().resolve("owner"),StandardCopyOption.ATOMIC_MOVE,StandardCopyOption.REPLACE_EXISTING);}
        finally{Files.deleteIfExists(temporary);}
    }
    /** Caller has already stopped serving and closed every database descriptor. Keeps the local claim. */
    public void releaseOwnership()throws IOException{
        synchronized(claim){synchronized(this){if(released)return;released=true;try{Files.deleteIfExists(bundle().resolve("owner"));}finally{try{presence.close();}finally{claim.owner=null;}}}}
    }
    @Override public void close()throws IOException{claim.close();}
}
