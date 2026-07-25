const chunks = [];
for await (const chunk of process.stdin) chunks.push(chunk);

const payload = JSON.parse(Buffer.concat(chunks).toString("utf8"));
const operations = payload.operations;
const results = payload.workload_result;
let failure;

if (!Array.isArray(operations) || !Array.isArray(results)) {
  failure = "verifier received no operation/result arrays";
} else if (operations.length !== results.length) {
  failure = "workload returned a different number of results";
} else {
  for (let index = 0; index < operations.length; index += 1) {
    const operation = operations[index];
    const result = results[index];
    if (
      result?.kind !== operation?.kind ||
      result?.byte_length !== operation?.byte_length ||
      result?.seed !== operation?.seed ||
      result?.checksum !== operation?.expected_checksum
    ) {
      failure = `result ${index} does not match the independent expectation`;
      break;
    }
  }
}

process.stdout.write(
  `${JSON.stringify(
    failure
      ? { success: false, failure_category: "incorrect_result", detail: failure }
      : { success: true },
  )}\n`,
);
