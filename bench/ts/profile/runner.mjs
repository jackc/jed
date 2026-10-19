// Host-independent sequencing: warm both paths, alternate order, validate every measured result.
export async function runCases({ metadata, warm, run, startCpu, stopCpu, saveCpu, settings, progress }) {
  const mode = settings.JED_PROFILE_MODE ?? "plain";
  const count = Number(settings.JED_PROFILE_SAMPLES ?? 7);
  const repeats = Number(settings.JED_PROFILE_CPU_REPEATS ?? 3);
  if (!["plain", "coarse", "detailed", "cpu"].includes(mode)) throw new Error("Invalid mode");
  if (![count, repeats].every((v) => Number.isSafeInteger(v) && v > 0)) throw new Error("Invalid sample counts");
  const cases = [];
  for (const workload of metadata.cases) {
    progress(`${workload.id}: warming`);
    const expected = await warm(workload.id, metadata.config.warmups);
    const samples = [];
    if (mode === "cpu") {
      for (const boundary of ["direct", "worker"]) {
        progress(`${workload.id}: ${boundary} CPU profile`);
        await startCpu();
        try {
          for (let i = 0; i < repeats; i++) samples.push(await run(workload.id, boundary, "fingerprint", false));
        } finally {
          await saveCpu(`${workload.id}-${boundary}`, await stopCpu());
        }
      }
    } else {
      for (const consumer of settings.JED_PROFILE_COUNT === "0" ? ["fingerprint"] : ["fingerprint", "count"]) {
        progress(`${workload.id}: ${consumer} ${count} samples`);
        for (let i = 0; i < count; i++) {
          const order = i % 2 ? ["worker", "direct"] : ["direct", "worker"];
          for (const boundary of order) samples.push(await run(workload.id, boundary, consumer, mode !== "plain"));
        }
      }
    }
    cases.push({ ...workload, expected, samples });
  }
  return { ...metadata, mode,
    profileConfig: { samplesPerBoundary: mode === "cpu" ? repeats : count,
      cpuRepeats: repeats, consumers: mode === "cpu" || settings.JED_PROFILE_COUNT === "0" ? ["fingerprint"] : ["fingerprint", "count"] },
    cases };
}
