import assert from "node:assert/strict"
import test from "node:test"

const encoder = new TextEncoder()
const decoder = new TextDecoder()
const calls = []

function output(context, decision, reason) {
  const hookSpecificOutput = {}
  if (context) hookSpecificOutput.additionalContext = context
  if (decision) hookSpecificOutput.permissionDecision = decision
  if (reason) hookSpecificOutput.permissionDecisionReason = reason
  return { hookSpecificOutput }
}

const spawnSync = (command, options) => {
  const event = command.at(-1)
  const payload = JSON.parse(decoder.decode(options.stdin))
  calls.push({ event, payload })

  let body
  if (event === "session-start") body = output("session context")
  if (event === "prompt-context") body = output("prompt context")
  if (event === "pre-edit") body = output("waiting message", "deny", "owned by ortak-2")
  if (event === "pre-bash") body = output("bash message")
  if (event === "post-bash") body = output("failure reminder")

  return {
    exitCode: 0,
    stdout: body ? encoder.encode(JSON.stringify(body)) : new Uint8Array(),
    stderr: new Uint8Array(),
  }
}

const { Ortak } = await import("../plugins/ortak/opencode/ortak.js")

test("OpenCode events use the maintained ortak hook contract", async () => {
  calls.length = 0
  const hooks = await Ortak(
    {
      directory: "/repo",
      client: { app: { log: async () => {} } },
    },
    { spawnSync },
  )

  const system = { system: [] }
  await hooks["experimental.chat.system.transform"]({ sessionID: "session-1" }, system)
  assert.deepEqual(system.system, ["session context\n\nprompt context"])
  assert.deepEqual(calls[0], {
    event: "session-start",
    payload: { session_id: "session-1", cwd: "/repo", harness: "opencode" },
  })

  await assert.rejects(
    hooks["tool.execute.before"](
      { tool: "edit", sessionID: "session-1", callID: "call-1" },
      {
        args: {
          filePath: "/repo/src/main.rs",
          oldString: "old",
          newString: "new",
          replaceAll: true,
        },
      },
    ),
    /owned by ortak-2/,
  )
  const edit = calls.find((call) => call.event === "pre-edit")
  assert.equal(edit.payload.tool_name, "OpenCodeEdit")
  assert.deepEqual(edit.payload.tool_input, {
    file_path: "/repo/src/main.rs",
    old_string: "old",
    new_string: "new",
    replace_all: true,
  })

  await hooks["tool.execute.before"](
    { tool: "bash", sessionID: "session-1", callID: "call-2" },
    { args: { command: "cargo test" } },
  )
  await hooks["tool.execute.after"](
    { tool: "bash", sessionID: "session-1", callID: "call-2", args: { command: "cargo test" } },
    { output: "test failed", metadata: { exit: 1 } },
  )
  const bashAfter = calls.find((call) => call.event === "post-bash")
  assert.deepEqual(bashAfter.payload.tool_response, {
    stdout: "test failed",
    stderr: "",
    exit_code: 1,
  })

  const nextSystem = { system: [] }
  await hooks["experimental.chat.system.transform"]({ sessionID: "session-1" }, nextSystem)
  assert.deepEqual(nextSystem.system, [
    "waiting message\n\nbash message\n\nfailure reminder\n\nprompt context",
  ])

  await hooks.event({ event: { type: "session.deleted", properties: { info: { id: "session-1" } } } })
  assert.equal(calls.at(-1).event, "session-end")
})
