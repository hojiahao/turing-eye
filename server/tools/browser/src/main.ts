import { readFile } from "node:fs/promises";
import { FrameReader, ProtocolFault, type Response } from "./protocol.js";
import { BrowserWorker, validateConfig, type ExperimentConfig } from "./worker.js";

async function main(): Promise<void> {
  const configPath = process.env.R1_BROWSER_CONFIG;
  if (!configPath) throw new Error("missing_r1_config");
  const configBytes = await readFile(configPath);
  if (configBytes.length > 32_768) throw new Error("config_too_large");
  const config = JSON.parse(configBytes.toString("utf8")) as ExperimentConfig;
  await validateConfig(config);
  let outputChain = Promise.resolve();
  const output = (frame: Response): Promise<void> => {
    const line = `${JSON.stringify(frame)}\n`;
    outputChain = outputChain.then(() => new Promise<void>((resolve, reject) => {
      process.stdout.write(line, (error) => error ? reject(error) : resolve());
    }));
    return outputChain;
  };
  const worker = new BrowserWorker(config, output);
  const reader = new FrameReader();
  let stopping: Promise<void> | undefined;
  function stop(reason: string, code: number): Promise<void> {
    if (stopping) return stopping;
    process.stdin.pause();
    if (reason !== "eof" && reason !== "signal") process.stderr.write(`r1_browser:${reason}\n`);
    stopping = (async () => {
      // A failed cleanup must be visible to the host, which owns process-tree recovery.
      const timeout = setTimeout(() => { process.stderr.write("r1_browser:cleanup_unconfirmed\n"); process.exit(2); }, 8000);
      try { await worker.shutdown(); await outputChain; process.exitCode = code; }
      catch { process.stderr.write("r1_browser:cleanup_failed\n"); process.exitCode = 2; }
      finally { clearTimeout(timeout); process.stdin.destroy(); process.exit(process.exitCode ?? 2); }
    })();
    return stopping;
  }
  process.stdin.on("data", (chunk: Buffer) => {
    if (stopping) return;
    try { reader.feed(chunk, (frame) => worker.accept(frame)); }
    catch (error) { void stop(error instanceof ProtocolFault ? error.reason : "transport_failed", 2); }
  });
  process.stdin.on("end", () => {
    if (stopping) return;
    try { reader.finish(); void stop("eof", 0); }
    catch { void stop("truncated_frame", 2); }
  });
  process.stdin.on("error", () => { void stop("input_closed", 2); });
  process.stdout.on("error", () => { void stop("output_closed", 2); });
  process.on("SIGTERM", () => { void stop("signal", 0); });
  process.on("SIGINT", () => { void stop("signal", 0); });
  process.stdin.resume();
}

await main().catch(() => { process.stderr.write("r1_browser:startup_failed\n"); process.exitCode = 2; });
