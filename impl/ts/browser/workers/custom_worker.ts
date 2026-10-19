import { startDatabaseWorker } from "jed-ts/browser/worker";
import { configure } from "./host_functions.ts";

startDatabaseWorker({ configure });
