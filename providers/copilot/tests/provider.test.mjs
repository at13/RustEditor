import test from "node:test";
import assert from "node:assert/strict";
import { runTurn, checkAuth, prompt } from "../dist/provider.js";

const request = (tools = []) => ({ api: 0, model: "auto", system: "You help edit a file.", messages: [{ role: "user", content: "Fix this" }], tools, max_output_tokens: 4096 });
function fake(action, authenticated = true) {
    const state = { started: 0, stopped: 0, deleted: [], aborted: 0, disconnected: 0, sends: 0 };
    const factory = () => ({
        start: async () => { state.started++; },
        getAuthStatus: async () => ({ isAuthenticated: authenticated }),
        stop: async () => { state.stopped++; return []; },
        forceStop: async () => {},
        deleteSession: async (id) => { state.deleted.push(id); },
        createSession: async (config) => {
            state.config = config;
            return { sessionId: "owned-session", abort: async () => { state.aborted++; }, disconnect: async () => { state.disconnected++; },
                sendAndWait: async (options) => { state.sends++; state.prompt = options.prompt; await action(config, state); } };
        },
    });
    return { state, factory };
}
const event = (type, data, id = "message-1") => ({ type, data, id });

test("ordinary prose streams through native SDK events without JSON parsing or duplicates", async () => {
    const { state, factory } = fake(async (c) => {
        c.onEvent(event("assistant.message_delta", { messageId: "message-1", deltaContent: "Hello " }));
        c.onEvent(event("assistant.message_delta", { messageId: "message-1", deltaContent: "😀" }));
        c.onEvent(event("assistant.message", { messageId: "message-1", content: "Hello 😀" }));
        c.onEvent(event("assistant.usage", { inputTokens: 20, outputTokens: 3, finishReason: "stop" }));
    });
    const emitted = [];
    await runTurn(request(), (e) => emitted.push(e), factory);
    assert.equal(emitted.filter(e => e.type === "text").map(e => e.delta).join(""), "Hello 😀");
    assert.deepEqual(emitted.at(-1), { type: "done", reason: "stop" });
    assert.deepEqual(emitted.find(e => e.type === "usage"), { type: "usage", input_tokens: 20, output_tokens: 3 });
    assert.equal(state.config.model, undefined); // runtime-selected model, not a bogus 'auto' model id
    assert.deepEqual(state.config.availableTools, []);
    assert.deepEqual(state.config.excludedTools, ["builtin:*", "mcp:*"]);
    assert.equal(state.config.enableFileHooks, false);
    assert.equal(state.config.enableConfigDiscovery, false);
    assert.equal(state.config.enableHostGitOperations, false);
    assert.equal(state.config.enableSkills, false);
    assert.equal(state.config.remoteSession, "off");
    assert.deepEqual(state.deleted, ["owned-session"]);
    assert.equal(state.stopped, 1);
});

test("native registered tools only queue proposals and terminate the SDK turn", async () => {
    const schema = { type: "object", properties: { edits: { type: "array" } }, required: ["edits"] };
    const args = { edits: [{ old_text: "a", new_text: "b" }] };
    const { state, factory } = fake(async (c) => {
        const tool = c.tools[0];
        assert.equal(tool.isTerminal, true);
        assert.equal(tool.skipPermission, true); // queueing isn't permission to run a host action
        assert.equal(tool.defer, "never");
        assert.deepEqual(tool.parameters, schema);
        const result = await tool.handler(args, { toolCallId: "native-call-123", toolName: "edit_buffer" });
        assert.equal(result.resultType, "success");
        assert.match(result.textResultForLlm, /NOT executed/);
        assert.equal((await c.onPermissionRequest({ kind: "write" })).kind, "reject");
    });
    const emitted = [];
    await runTurn(request([{ name: "edit_buffer", description: "Propose edits", input_schema: schema }]), e => emitted.push(e), factory);
    assert.deepEqual(emitted, [{ type: "tool_call", id: "native-call-123", name: "edit_buffer", arguments: args }, { type: "done", reason: "tool_calls" }]);
    assert.deepEqual(state.config.availableTools, ["custom:rusteditor_tool_0"]);
    assert.equal(state.config.tools[0].name, "rusteditor_tool_0");
    assert.match(state.config.systemMessage.content, /edit_buffer => rusteditor_tool_0/);
    assert.equal(state.sends, 1); // no SDK-generated second turn with a fake tool result
});

test("host acceptance/rejection results are preserved in continuation context", () => {
    const r = request();
    r.messages.push({ role: "assistant", content: "Proposing", tool_calls: [{ id: "c", name: "edit_buffer", arguments: {} }] });
    r.messages.push({ role: "tool", tool_call_id: "c", content: "ERROR: The user rejected this action." });
    const context = prompt(r);
    assert.match(context, /rejected this action/);
    assert.match(context, /only proposals/);
    assert.match(context, /tool_call_id/);
});

test("no authentication means no model call; only an owned client is stopped", async () => {
    const { state, factory } = fake(() => assert.fail("must not send"), false);
    const emitted = [];
    await runTurn(request(), e => emitted.push(e), factory);
    assert.equal(state.sends, 0);
    assert.equal(state.deleted.length, 0);
    assert.equal(state.stopped, 1);
    assert.equal(emitted.at(-1).type, "error");
    assert.match(emitted.at(-1).message, /not signed in/);
    assert.equal(await checkAuth(factory), false);
    assert.equal(state.sends, 0);
});

test("SDK errors don't leak diagnostic/document text and cleanup precedes terminal output", async () => {
    const { state, factory } = fake(() => { throw new Error("private document / token must not be logged"); });
    const emitted = [];
    await runTurn(request(), e => { if (e.type === "error") assert.equal(state.stopped, 1); emitted.push(e); }, factory);
    assert.equal(emitted.at(-1).type, "error");
    assert.doesNotMatch(JSON.stringify(emitted), /private document|token must/);
    assert.equal(state.aborted, 1);
    assert.equal(state.disconnected, 1);
});

test("abort and invalid/duplicate declarations cannot execute a native model turn", async () => {
    const controller = new AbortController(); controller.abort();
    const { state, factory } = fake(() => assert.fail("must not send"));
    const emitted = [];
    await runTurn(request(), e => emitted.push(e), factory, controller.signal);
    assert.equal(state.sends, 0);
    assert.match(emitted.at(-1).message, /cancelled/);
    const tool = { name: "read_buffer", description: "Read", input_schema: {} };
    await assert.rejects(runTurn(request([tool, tool]), () => {}, factory), /duplicate/);
});
