use std::{
    env,
    io::{self, Read, Write},
    net::{TcpListener, TcpStream},
    process::ExitCode,
    thread,
};

const DOCUMENT: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>bperf browser operation fixture</title>
<script type="module">
const configuration = {
  cpuRounds: __CPU_ROUNDS__,
  retainedObjects: __RETAINED_OBJECTS__,
};
let retained = [];
let cpuSink = 0;

function checksum(byteLength, seed) {
  let value = 2166136261;
  for (let index = 0; index < byteLength; index += 1) {
    const byte = (seed + Math.imul(index, 31)) & 255;
    value = Math.imul(value ^ byte, 16777619) >>> 0;
  }
  return value;
}

globalThis.__bperf = {
  run(operation) {
    if (
      operation?.kind !== "parse-fragment" ||
      !Number.isSafeInteger(operation.byte_length) ||
      !Number.isSafeInteger(operation.seed)
    ) {
      throw new Error("Unsupported fixture operation");
    }

    let sink = 0;
    for (let round = 0; round < configuration.cpuRounds; round += 1) {
      sink ^= checksum(operation.byte_length, operation.seed + (round & 7));
    }
    cpuSink ^= sink;

    retained = Array.from(
      { length: configuration.retainedObjects },
      (_, index) => ({
        index,
        label: "fragment-" + index,
        offsets: [index, index + 8, index + 16, index + 24],
      }),
    );

    return {
      kind: operation.kind,
      byte_length: operation.byte_length,
      seed: operation.seed,
      checksum: checksum(operation.byte_length, operation.seed),
    };
  },
  async settle() {
    await Promise.resolve(cpuSink);
  },
};
</script>
"#;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("advanced adapter failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> io::Result<()> {
    let (cpu_rounds, retained_objects) = match env::args().nth(1).as_deref() {
        Some("baseline") => (360_000, 18_000),
        Some("candidate") => (240_000, 8_000),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected variant argument `baseline` or `candidate`",
            ));
        }
    };
    let document = DOCUMENT
        .replace("__CPU_ROUNDS__", &cpu_rounds.to_string())
        .replace("__RETAINED_OBJECTS__", &retained_objects.to_string())
        .into_bytes();
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    println!(r#"{{"protocol_version":2,"url":"http://127.0.0.1:{port}/"}}"#);
    io::stdout().flush()?;

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let document = document.clone();
                thread::spawn(move || {
                    if let Err(error) = serve(stream, &document) {
                        eprintln!("request failed: {error}");
                    }
                });
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
    Ok(())
}

fn serve(mut stream: TcpStream, document: &[u8]) -> io::Result<()> {
    let mut request = [0_u8; 8192];
    let length = stream.read(&mut request)?;
    let root_request = request[..length].starts_with(b"GET / HTTP/");
    let (status, content_type, body): (&str, &str, &[u8]) = if root_request {
        ("200 OK", "text/html; charset=utf-8", document)
    } else {
        ("404 Not Found", "text/plain; charset=utf-8", b"not found")
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}
