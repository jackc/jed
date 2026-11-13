//! Opt-in interchange transport. SQL and files remain public host API inputs.
use jed::{
    Argon2Params, AttachSource, CreateOptions, Credential, Database, EngineError, Environment,
    HostFunction, Locking, OpenOptions, ScalarType, ScratchEncryption, SqlState, Value, Volatility,
};
use std::{
    error::Error,
    io::{self, BufRead, Write},
    path::PathBuf,
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn bytes(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 || !s.is_ascii() {
        return Err("invalid hex".into());
    };
    (0..s.len())
        .step_by(2)
        .map(|i| Ok(u8::from_str_radix(&s[i..i + 2], 16)?))
        .collect()
}
fn text(s: &str) -> Result<String> {
    Ok(String::from_utf8(bytes(s)?)?)
}
fn credential(kind: &str, encoded: &str) -> Result<Option<Credential>> {
    Ok(match kind {
        "plain" => None,
        "pass" => Some(Credential::Passphrase(text(encoded)?)),
        "key" => Some(Credential::Key(
            bytes(encoded)?
                .try_into()
                .map_err(|_| "invalid key length")?,
        )),
        _ => return Err("invalid credential kind".into()),
    })
}
fn failure(error: &(dyn Error + 'static)) {
    let code = error
        .downcast_ref::<EngineError>()
        .map(|e| e.code())
        .unwrap_or("XXXXX");
    println!("ERR\t{}\t{}", code, hex(error.to_string().as_bytes()));
    io::stdout().flush().unwrap();
}
fn main() {
    if let Err(error) = run() {
        failure(error.as_ref());
        std::process::exit(1)
    }
}
fn run() -> Result<()> {
    let a: Vec<_> = std::env::args().skip(1).collect();
    if a.len() != 6 {
        return Err(
            "expected action path fixtures credential-kind credential-hex environment".into(),
        );
    }
    let mut builder = Environment::builder();
    if a[5] != "empty" {
        builder = builder
            .load_unicode_data(&std::fs::read(PathBuf::from(&a[2]).join("unicode.jucd"))?)?
            .load_time_zone_data(&std::fs::read(PathBuf::from(&a[2]).join("tzdata.jtz"))?)?;
    }
    if a[5] == "full" {
        builder = builder.register_function(
            HostFunction::new(
                "geo_hash",
                vec![ScalarType::Int64],
                ScalarType::Int64,
                Box::new(|v| {
                    let Value::Int(n) = v[0] else {
                        return Err(EngineError::new(
                            SqlState::DatatypeMismatch,
                            "expected host integer",
                        ));
                    };
                    Ok(Value::Int(n.checked_mul(10).ok_or_else(|| {
                        EngineError::new(SqlState::NumericValueOutOfRange, "host overflow")
                    })?))
                }),
            )
            .volatility(Volatility::Immutable)
            .cross_core(true)
            .component_id("com.example/geo_hash")
            .semantic_version(1),
        )?;
    }
    let environment = builder.insecure_seeded_key_entropy(0x4a454442).build();
    let cred = credential(&a[3], &a[4])?;
    let kdf = Argon2Params {
        memory_kib: 32,
        iterations: 1,
        parallelism: 1,
    };
    let mut database = match a[0].as_str() {
        "create" => Database::create(CreateOptions {
            path: Some(PathBuf::from(&a[1])),
            page_size: 1024,
            environment,
            credential: cred,
            kdf,
            locking: Locking::None,
            scratch_encryption: ScratchEncryption::On,
            ..Default::default()
        })?,
        "open" => Database::open_with_options(
            &a[1],
            OpenOptions {
                environment,
                credential: cred,
                locking: Locking::None,
                scratch_encryption: ScratchEncryption::On,
                ..Default::default()
            },
        )?,
        _ => return Err("invalid action".into()),
    };
    println!("READY");
    io::stdout().flush()?;
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.len() > 16 * 1024 * 1024 {
            return Err("interop command too large".into());
        };
        let f: Vec<_> = line.split('\t').collect();
        let arity = match f[0] {
            "EXEC" | "QUERY" => 2,
            "ATTACH" => 4,
            "REWRAP" => 3,
            "UPGRADE" | "CLOSE" => 1,
            _ => 0,
        };
        if f.len() != arity {
            failure(&EngineError::new(
                SqlState::InvalidParameterValue,
                "invalid command arity",
            ));
            continue;
        }
        if f[0] == "CLOSE" {
            database.close()?;
            println!("OK\t");
            io::stdout().flush()?;
            return Ok(());
        }
        let response: Result<String> = (|| match f[0] {
            "EXEC" => {
                database.execute(&text(f.get(1).ok_or("missing SQL")?)?, &[])?;
                Ok(String::new())
            }
            "QUERY" => {
                let mut rows = database.query(&text(f.get(1).ok_or("missing SQL")?)?, &[])?;
                let mut out = Vec::new();
                for row in rows.by_ref() {
                    let fields: Result<Vec<_>> = row?
                        .into_iter()
                        .map(|v| match v {
                            Value::Null => Ok("-".into()),
                            Value::Text(s) => Ok(hex(s.as_bytes())),
                            _ => Err("interop QUERY requires text fields".into()),
                        })
                        .collect();
                    out.push(fields?.join(":"));
                }
                Ok(format!("{};{}", out.len(), out.join(",")))
            }
            "ATTACH" => {
                if f.len() != 4 {
                    return Err("invalid attach".into());
                };
                let mut source = AttachSource::file(text(f[1])?).locking(Locking::None);
                if let Some(c) = credential(f[2], f[3])? {
                    source = source.credential(c)
                };
                database.attach("aux", source, false)?;
                Ok(String::new())
            }
            "REWRAP" => {
                database.change_credential(
                    "main",
                    credential(
                        f.get(1).ok_or("missing kind")?,
                        f.get(2).ok_or("missing credential")?,
                    )?
                    .ok_or("missing credential")?,
                    kdf,
                )?;
                Ok(String::new())
            }
            "UPGRADE" => {
                database.upgrade_collations()?;
                Ok(String::new())
            }
            _ => Err("unknown interop command".into()),
        })();
        match response {
            Ok(v) => println!("OK\t{v}"),
            Err(e) => failure(e.as_ref()),
        };
        io::stdout().flush()?;
    }
    database.close()?;
    Ok(())
}
