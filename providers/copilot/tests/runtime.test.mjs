// Optional real SDK/runtime handshake smoke test. No credentials, auth query,
// model request, tools or user configuration are used.
import test from "node:test";
import assert from "node:assert/strict";
import { CopilotClient, RuntimeConnection } from "@github/copilot-sdk";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

test("official SDK connects to its bundled runtime over stdio without authentication", {
    skip: process.env.RUSTEDITOR_SDK_RUNTIME_SMOKE !== "1", timeout: 60_000,
}, async () => {
    const dir = await mkdtemp(join(tmpdir(), "re-copilot-sdk-smoke-"));
    const env = { ...process.env, COPILOT_HOME: dir, COPILOT_TELEMETRY_DISABLED: "1" };
    for (const name of ["GH_TOKEN", "GITHUB_TOKEN", "COPILOT_GITHUB_TOKEN", "COPILOT_CLI_PATH", "COPILOT_CLI_URL", "COPILOT_PROVIDER_BASE_URL", "COPILOT_PROVIDER_API_KEY"]) delete env[name];
    const client = new CopilotClient({ connection: RuntimeConnection.forStdio(), mode: "empty", baseDirectory: dir,
        workingDirectory: dir, useLoggedInUser: false, env, logLevel: "none" });
    try {
        await client.start();
        const status = await client.getStatus();
        assert.equal(typeof status.version, "string");
        assert.equal(typeof status.protocolVersion, "number");
    } finally {
        try { await client.stop(); } finally { await client.forceStop(); await rm(dir, { recursive: true, force: true }); }
    }
});
