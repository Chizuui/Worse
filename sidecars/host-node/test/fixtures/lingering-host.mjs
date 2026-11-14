// Mimics the real Host: on dispose it calls process.exit while it still owns a ref'd handle.
// HARNESS_TEST_LINGER=timer leaves a ref'd interval, which cannot be released generically.
import { createServer } from "node:net";

const { parentPort } = process;
const server = createServer();
server.listen(0, "127.0.0.1");

parentPort.on("message", (event) => {
  if (event.data?.type !== "dispose") return;
  if (process.env.HARNESS_TEST_LINGER === "timer") setInterval(() => {}, 60_000);
  process.exit(7);
  // process.exit now returns: the launcher drains the loop instead of tearing down V8 mid-flight.
  process.stderr.write("lingering host: exit returned\n");
});
