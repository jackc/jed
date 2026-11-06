// Opt-in interchange transport. The public synchronous tooling host executes unchanged SQL.
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { createInterface } from "node:readline";
const { createDatabase, openDatabase, EnvironmentBuilder, EngineError, attachFile, intValue } = await import(process.env["JED_INTEROP_TS_TOOLING"]!);
const args=process.argv.slice(2);
function decode(input:string):string {
 if(!/^(?:[0-9a-fA-F]{2})*$/.test(input))throw Error("invalid hex input");
 return new TextDecoder("utf-8",{fatal:true}).decode(Buffer.from(input,"hex"));
}
function credential(kind:string,encoded:string){
 if(kind==="plain")return undefined;
 if(kind==="pass")return {passphrase:decode(encoded)};
 if(kind==="key"&&/^[0-9a-fA-F]{64}$/.test(encoded))return {key:new Uint8Array(Buffer.from(encoded,"hex"))};
 throw Error("invalid credential");
}
function fail(error:unknown){const code=error instanceof EngineError?error.code():"XXXXX";console.log(`ERR\t${code}\t${Buffer.from(String(error)).toString("hex")}`)}
async function run(){
 if(args.length!==6)throw Error("expected action path fixtures credential-kind credential-hex environment");
 const [action,path,fixtures,kind,encoded,mode]=args as [string,string,string,string,string,string];
 const builder=new EnvironmentBuilder();
 if(mode!=="empty"){builder.loadUnicodeData(new Uint8Array(readFileSync(join(fixtures,"unicode.jucd"))));builder.loadTimeZoneData(new Uint8Array(readFileSync(join(fixtures,"tzdata.jtz"))));}
 if(mode==="full")builder.registerFunction({name:"geo_hash",argTypes:["i64"],result:"i64",cost:1n,volatility:"immutable",crossCore:true,componentId:"com.example/geo_hash",semanticVersion:1,kernel:(v)=>{
   if(v[0]?.kind!=="int")throw Error("expected host integer");return intValue(v[0].int*10n);
 }});
 const environment=builder.insecureSeededKeyEntropy(0x4a454442n).build();
 const kdf={memoryKib:32,iterations:1,parallelism:1};const options={environment,credential:credential(kind,encoded),locking:"none" as const,scratchEncryption:"on" as const};
 const database=action==="create"?createDatabase({path,pageSize:1024,kdf,...options}):action==="open"?openDatabase(path,options):(()=>{throw Error("invalid action")})();
 console.log("READY");
 const input=createInterface({input:process.stdin,crlfDelay:Infinity});
 try{for await(const line of input){
  if(Buffer.byteLength(line)>16*1024*1024)throw Error("interop command too large");const f=line.split("\t");
  try{const arity:Record<string,number>={EXEC:2,QUERY:2,ATTACH:4,REWRAP:3,UPGRADE:1,CLOSE:1};if(arity[f[0]!]!==f.length)throw Error("invalid command arity");let result="";switch(f[0]){
   case "EXEC":database.execute(decode(f[1]!));break;
   case "QUERY":{const rows=database.query(decode(f[1]!));const out:string[]=[];try{for(const row of rows){out.push(row.map(v=>{if(v.kind==="null")return "-";if(v.kind!=="text")throw Error("interop QUERY requires text fields");return Buffer.from(v.text).toString("hex")}).join(":"))}}finally{rows.close()};result=`${out.length};${out.join(",")}`;break;}
   case "ATTACH":if(f.length!==4)throw Error("invalid attach");database.attach("aux",attachFile(decode(f[1]!),{locking:"none",credential:credential(f[2]!,f[3]!)}),false);break;
   case "REWRAP":{const c=credential(f[1]!,f[2]!);if(!c)throw Error("missing credential");database.changeCredential("main",c,kdf);break;}
   case "UPGRADE":database.upgradeCollations();break;
   case "CLOSE":database.close();console.log("OK\t");return;
   default:throw Error("unknown interop command");
  };console.log(`OK\t${result}`)}catch(error){fail(error)}
 }}finally{input.close();process.stdin.pause();database.close()}
}
try{await run()}catch(error){fail(error);process.exit(1)}
