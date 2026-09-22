/**
 * Thumbrella Typescript client example
 * 
 * This is an overly simplified example of using the Typescript client for Thumbrella.
 * The actual Typescript client lives in
 * 
 * - **Npmjs** at https://www.npmjs.com/package/@thumbrella/client
 * - **Github** at https://github.com/thumbrella-dev/clients/
 * 
 * See the more complete Typescript client examples at
 * https://github.com/thumbrella-dev/clients/tree/main/typescript/examples
 * 
 */

import { writeFileSync } from "node:fs";
import { Client } from "@thumbrella/client";


async function main(): Promise<void> {
  // Client uses `$TBR_CONNECT` to define the server url or Cloud token
  const tbr = await new Client();

  
  // Generate a single specific thumbnail from url
  const url = "https://demo.thumbrella.dev/media/golden-gate.exr";
  const result = await tbr.thumb(url);
  const media = result.verify().media
  console.log(`${media.kind} ${media.file_size} bytes -> {m.thumbnail.length} bytes`);

  
  // Write thumbnail to disk
  writeFileSync("/tmp/thumbnail.jpeg", media.thubmnail.bytes);
}
