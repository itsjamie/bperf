#!/usr/bin/env node

import readline from "node:readline";

import {
  handleRequest,
  invalidRequest,
  type WireResponse,
} from "./protocol.ts";

function writeResponse(response: WireResponse): Promise<void> {
  return new Promise((resolve, reject) => {
    process.stdout.write(
      `${JSON.stringify(response)}\n`,
      (error) => (error ? reject(error) : resolve()),
    );
  });
}

const input = readline.createInterface({
  input: process.stdin,
  crlfDelay: Infinity,
});

for await (const line of input) {
  let response: WireResponse;
  let terminate = false;
  try {
    const result = await handleRequest(JSON.parse(line) as unknown);
    response = result.response;
    terminate = result.terminate;
  } catch (error) {
    response = invalidRequest(error);
  }

  await writeResponse(response);
  if (terminate) {
    // No request may observe a process after its shutdown acknowledgement.
    process.exit(0);
  }
}
