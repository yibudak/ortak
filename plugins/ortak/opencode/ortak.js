// ortak-version: __ORTAK_VERSION__

const decoder = new TextDecoder()

function hookContext(result) {
  return result?.hookSpecificOutput?.additionalContext
}

function sessionIdFromEvent(event) {
  const properties = event?.properties
  return properties?.info?.id ?? properties?.sessionID ?? properties?.sessionId ?? properties?.id
}

function adaptTool(tool, args) {
  if (tool === "edit") {
    return {
      event: "edit",
      name: "OpenCodeEdit",
      input: {
        file_path: args.filePath,
        old_string: args.oldString,
        new_string: args.newString,
        replace_all: args.replaceAll,
      },
    }
  }
  if (tool === "write") {
    return {
      event: "edit",
      name: "Write",
      input: { file_path: args.filePath, content: args.content },
    }
  }
  if (tool === "apply_patch") {
    return {
      event: "edit",
      name: "apply_patch",
      input: { patch: args.patchText },
    }
  }
  if (tool === "bash") {
    return {
      event: "bash",
      name: "Bash",
      input: { command: args.command },
    }
  }
}

export const Ortak = async ({ client, directory }, options = {}) => {
  const spawnSync = typeof options.spawnSync === "function" ? options.spawnSync : Bun.spawnSync
  const active = new Set()
  const pending = new Map()
  let warned = false

  const warn = async (message) => {
    if (warned) return
    warned = true
    try {
      await client.app.log({
        body: {
          service: "ortak",
          level: "warn",
          message,
        },
      })
    } catch {
      // The adapter must not break OpenCode when logging is unavailable.
    }
  }

  const runHook = async (event, payload) => {
    try {
      const result = spawnSync(["ortak", "hook", event], {
        cwd: directory,
        env: process.env,
        stdin: new TextEncoder().encode(JSON.stringify(payload)),
        stdout: "pipe",
        stderr: "pipe",
      })
      const stdout = decoder.decode(result.stdout).trim()
      const stderr = decoder.decode(result.stderr).trim()
      if (result.exitCode !== 0) {
        await warn(`ortak hook ${event} failed: ${stderr || `exit ${result.exitCode}`}`)
        return
      }
      if (!stdout) {
        if (stderr) await warn(stderr)
        return
      }
      return JSON.parse(stdout)
    } catch (error) {
      await warn(`could not run ortak hook ${event}: ${error instanceof Error ? error.message : String(error)}`)
    }
  }

  const payload = (sessionID, extra = {}) => ({
    session_id: sessionID,
    cwd: directory,
    harness: "opencode",
    ...extra,
  })

  const queue = (sessionID, context) => {
    if (!context) return
    const items = pending.get(sessionID) ?? []
    items.push(context)
    pending.set(sessionID, items)
  }

  const takeQueued = (sessionID) => {
    const items = pending.get(sessionID) ?? []
    pending.delete(sessionID)
    return items
  }

  const ensureStarted = async (sessionID) => {
    if (active.has(sessionID)) return
    const result = await runHook("session-start", payload(sessionID))
    if (!result) return
    active.add(sessionID)
    return hookContext(result)
  }

  return {
    "experimental.chat.system.transform": async ({ sessionID }, output) => {
      if (!sessionID) return
      const context = []
      const started = await ensureStarted(sessionID)
      if (started) context.push(started)
      context.push(...takeQueued(sessionID))
      const prompt = await runHook("prompt-context", payload(sessionID))
      const promptContext = hookContext(prompt)
      if (promptContext) context.push(promptContext)
      if (context.length) output.system.push(context.join("\n\n"))
    },

    "tool.execute.before": async (input, output) => {
      const tool = adaptTool(input.tool, output.args)
      if (!tool) return
      queue(input.sessionID, await ensureStarted(input.sessionID))
      const result = await runHook(
        tool.event === "edit" ? "pre-edit" : "pre-bash",
        payload(input.sessionID, { tool_name: tool.name, tool_input: tool.input }),
      )
      queue(input.sessionID, hookContext(result))
      const decision = result?.hookSpecificOutput?.permissionDecision
      if (decision === "deny") {
        throw new Error(result.hookSpecificOutput.permissionDecisionReason ?? "The ortak gate denied this tool call.")
      }
    },

    "tool.execute.after": async (input, output) => {
      const tool = adaptTool(input.tool, input.args)
      if (!tool) return
      const toolResponse = {
        stdout: output.output ?? "",
        stderr: "",
        exit_code: output.metadata?.exit,
      }
      const result = await runHook(
        tool.event === "edit" ? "post-edit" : "post-bash",
        payload(input.sessionID, {
          tool_name: tool.name,
          tool_input: tool.input,
          tool_response: toolResponse,
        }),
      )
      queue(input.sessionID, hookContext(result))
    },

    event: async ({ event }) => {
      if (event.type !== "session.deleted") return
      const sessionID = sessionIdFromEvent(event)
      if (!sessionID || !active.delete(sessionID)) return
      pending.delete(sessionID)
      await runHook("session-end", payload(sessionID))
    },

    dispose: async () => {
      await Promise.all(
        [...active].map((sessionID) => runHook("session-end", payload(sessionID))),
      )
      active.clear()
      pending.clear()
    },
  }
}
