// Mimics the real Host writing diagnostics to stdout at module load and on dispose.
const { parentPort } = process;
console.log("noisy host: stdout diagnostics must be redirected to stderr");
process.stdout.write("raw stdout write\n");
parentPort.on("message", (event) => {
  if (event.data?.type === "dispose") {
    console.log("noisy host: disposing");
    process.exit(0);
  }
});
