const childProcess = require("node:child_process");
const path = require("node:path");
const { pathToFileURL } = require("node:url");
const { createRequire } = require("node:module");

const originalExec = childProcess.exec;
childProcess.exec = function patchedExec(...args) {
  try {
    return originalExec.apply(this, args);
  } catch (error) {
    if (error?.code !== "EPERM") throw error;
    const callback = args.find((arg) => typeof arg === "function");
    if (callback) queueMicrotask(() => callback(error, "", ""));
    return { on() { return this; }, kill() {}, unref() {} };
  }
};

const requireFromApp = createRequire(path.join(__dirname, "..", "package.json"));
const viteBin = path.join(
  path.dirname(requireFromApp.resolve("vite/package.json")),
  "bin",
  "vite.js",
);

import(pathToFileURL(viteBin).href).catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
