import { checkAuth, runTurn, type Request, type Event } from "./provider.js";

const emit = (event: Event): void => { process.stdout.write(`${JSON.stringify(event)}\n`); };
const controller = new AbortController();
process.once("SIGTERM", () => controller.abort());
process.once("SIGINT", () => controller.abort());
try {
    if (process.argv.includes("--check-auth")) {
        process.stdout.write(`${JSON.stringify({ is_authenticated: await checkAuth() })}\n`);
    } else {
        const chunks: Buffer[] = [];
        let bytes = 0;
        for await (const chunk of process.stdin) {
            const data = Buffer.from(chunk);
            bytes += data.length;
            if (bytes > 32 * 1024 * 1024) throw new Error("Request too large");
            chunks.push(data);
        }
        // Decode once: UTF-8 characters may straddle pipe chunks.
        const text = Buffer.concat(chunks).toString("utf8");
        await runTurn(JSON.parse(text) as Request, emit, undefined, controller.signal);
    }
} catch {
    emit({ type: "error", message: "Cannot start Copilot SDK provider. Check the SDK installation and Node.js version." });
    process.exitCode = 1;
}
