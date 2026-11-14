package org.jed.host;

import java.io.IOException;
import java.net.UnixDomainSocketAddress;
import java.nio.channels.ServerSocketChannel;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.nio.file.Path;
import java.nio.file.attribute.PosixFilePermissions;
import java.util.HexFormat;

/** Same-user endpoint permissions are verified before binding; no PID or pathname grants ownership. */
public final class PageEndpoint implements AutoCloseable {
    private final Path path;
    private final ServerSocketChannel server;
    private PageEndpoint(Path path,ServerSocketChannel server){this.path=path;this.server=server;}
    public Path path(){return path;}public ServerSocketChannel server(){return server;}
    public static PageEndpoint bind(ExclusiveFileLock owner)throws IOException{
        long uid=ExclusiveFileLock.uid();Path directory=owner.bundle().resolve("uid-"+uid);Path endpoint=directory.resolve("endpoint");
        if(!fits(endpoint)||!privateDirectory(directory,uid,true)){
            String suffix="jed-"+HexFormat.of().formatHex(owner.incarnation(),0,8)+".sock";
            String runtime=System.getenv("XDG_RUNTIME_DIR");Path candidate=runtime==null||runtime.isEmpty()?null:Path.of(runtime);
            if(candidate!=null&&candidate.isAbsolute()&&privateDirectory(candidate,uid,false)&&fits(candidate.resolve(suffix)))endpoint=candidate.resolve(suffix);
            else{candidate=Path.of("/tmp/jed-"+uid);if(!privateDirectory(candidate,uid,true)||!fits(candidate.resolve(suffix)))return null;endpoint=candidate.resolve(suffix);}
        }
        ServerSocketChannel server=ServerSocketChannel.open(java.net.StandardProtocolFamily.UNIX);boolean success=false;
        try{Files.deleteIfExists(endpoint);owner.publishEndpoint(endpoint.toString());server.bind(UnixDomainSocketAddress.of(endpoint));success=true;return new PageEndpoint(endpoint,server);}
        finally{if(!success){try{server.close();}finally{Files.deleteIfExists(endpoint);owner.publishEndpoint("");}}}
    }
    private static boolean fits(Path path){return path.toString().getBytes(java.nio.charset.StandardCharsets.UTF_8).length<=100;}
    private static boolean privateDirectory(Path path,long uid,boolean create)throws IOException{
        if(create)try{Files.createDirectory(path,PosixFilePermissions.asFileAttribute(PosixFilePermissions.fromString("rwx------")));}catch(java.nio.file.FileAlreadyExistsException exists){}
        if(!Files.isDirectory(path,LinkOption.NOFOLLOW_LINKS))return false;
        long actual=((Number)Files.getAttribute(path,"unix:uid",LinkOption.NOFOLLOW_LINKS)).longValue();
        int mode=((Number)Files.getAttribute(path,"unix:mode",LinkOption.NOFOLLOW_LINKS)).intValue();
        return actual==uid&&(mode&07777)==0700;
    }
    public void close()throws IOException{try{server.close();}finally{Files.deleteIfExists(path);}}
}
