import http from "node:http";

function benchmarkPage({ cpuRounds, retainedObjects }) {
  const configuration = JSON.stringify({ cpuRounds, retainedObjects });
  return `<!doctype html>
<meta charset="utf-8">
<title>bperf browser operation fixture</title>
<script type="module">
const configuration = ${configuration};
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
</script>`;
}

export function startVariant(configuration) {
  const document = benchmarkPage(configuration);
  const server = http.createServer((request, response) => {
    if (request.url !== "/") {
      response.writeHead(404).end();
      return;
    }
    response.writeHead(200, {
      "content-type": "text/html; charset=utf-8",
      "cache-control": "no-store",
    });
    response.end(document);
  });

  server.listen(0, "127.0.0.1", () => {
    const address = server.address();
    if (!address || typeof address === "string") {
      throw new Error("Fixture server did not expose a TCP port");
    }
    process.stdout.write(
      `${JSON.stringify({
        protocol_version: 1,
        url: `http://127.0.0.1:${address.port}/`,
      })}\n`,
    );
  });

  process.on("SIGTERM", () => server.close(() => process.exit(0)));
  process.on("SIGINT", () => server.close(() => process.exit(0)));
}
