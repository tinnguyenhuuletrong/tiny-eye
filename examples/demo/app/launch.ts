// Starts both services of the demo shop, each with its own OTel service name.
const run = (service: string, port: number) =>
  Bun.spawn([process.execPath, "server.ts"], {
    stdio: ["inherit", "inherit", "inherit"],
    env: { ...process.env, SERVICE: service, PORT: String(port), OTEL_SERVICE_NAME: service },
  });

const children = [run("payments", 3001), run("shop-api", 3000)];
const shutdown = () => {
  for (const c of children) c.kill("SIGTERM");
  setTimeout(() => process.exit(0), 1500);
};
process.on("SIGINT", shutdown);
process.on("SIGTERM", shutdown);
