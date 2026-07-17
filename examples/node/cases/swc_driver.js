const fs = require("node:fs");
const path = require("node:path");

const [swcModule, sourcePath] = process.argv.slice(2);
if (!swcModule || !sourcePath) {
  console.error("usage: swc_driver.js <swc-module> <source>");
  process.exit(2);
}

const swc = require(path.resolve(swcModule));
const source = fs.readFileSync(sourcePath, "utf8");

try {
  const result = swc.transformSync(source, {
    filename: sourcePath,
    jsc: {
      parser: { syntax: "typescript" },
      target: "es2022",
    },
  });
  process.stdout.write(result.code);
} catch (error) {
  console.error(String(error));
  process.exitCode = 1;
}
