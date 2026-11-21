// Starts both services of the demo shop, each with its own OTel service name.
import { spawn } from "node:child_process";

const run = (service, port) =>
  spawn(process.execPath, ["--import", "./telemetry.js", "server.js"], {
    stdio: "inherit",
    env: { ...process.env, SERVICE: service, PORT: String(port), OTEL_SERVICE_NAME: service },
  });

const children = [run("payments", 3001), run("shop-api", 3000)];
const stop = () => { children.forEach(c => c.kill("SIGTERM")); setTimeout(() => process.exit(0), 1500); };
process.on("SIGINT", stop);
process.on("SIGTERM", stop);
