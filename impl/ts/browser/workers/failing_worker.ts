import { startDatabaseWorker } from "jed-ts/browser/worker";

startDatabaseWorker({
  async configure() {
    await new Promise<void>((resolve) => setTimeout(resolve, 10));
    throw new Error("application configuration failed");
  },
});
