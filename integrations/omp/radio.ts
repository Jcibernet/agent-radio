/**
 * agent-radio as a native omp (https://omp.sh) custom tool.
 *
 * Thin, schema'd wrapper over the `agent-radio` binary (the single source of
 * truth: file locking, secret scanning, notify flags all live there). This
 * exists to kill the bash-quoting tax on radio bodies (quotes, accents, long
 * strings) and give the model structured params.
 *
 * Install: copy into `.omp/tools/` in your repo. If the binary is not on
 * PATH, set AGENT_RADIO_BIN to its location.
 */
import type { CustomToolFactory } from "@oh-my-pi/pi-coding-agent";

const KINDS = [
	"ASK",
	"FYI",
	"DONE",
	"ACK",
	"DECLINE",
	"FAILURE",
	"BLOCKED",
	"RISK",
	"REVIEW_REQUEST",
	"HANDOFF",
] as const;

const REPLY_OPS = ["ack", "done", "decline", "failure"] as const;

const factory: CustomToolFactory = (pi) => {
	const z = pi.zod;
	return {
		name: "radio",
		label: "Agent Radio",
		description: [
			"Local agent radio (agent-radio binary) for coordinating with the other agents",
			"working this worktree (opencode, droid, ...). Ops:",
			"send (requires to+body; kind defaults ASK),",
			"inbox (unread for me; peek=true to not mark read),",
			"history (recent traffic; limit/with filters),",
			"ack/done/decline/failure (reply to a numbered message from the last inbox/history view),",
			"team (known agents), status (unread count), wait (block until a message arrives).",
			"Bodies are plain prose, no secrets (the script rejects credential-looking text).",
		].join(" "),
		parameters: z.object({
			op: z.enum([
				"send",
				"inbox",
				"history",
				...REPLY_OPS,
				"team",
				"status",
				"wait",
			]),
			as: z
				.string()
				.optional()
				.describe("agent identity; defaults to 'claude'"),
			to: z.string().optional().describe("send: recipient agent or 'all'"),
			kind: z
				.enum(KINDS)
				.optional()
				.describe("send: message kind (default ASK)"),
			body: z
				.string()
				.optional()
				.describe("send/replies: message body, plain prose"),
			focus: z
				.array(z.string())
				.optional()
				.describe("send: concrete files/paths this message is about"),
			risk: z.string().optional().describe("send: one-line risk note"),
			priority: z.enum(["low", "normal", "high", "urgent"]).optional(),
			number: z
				.number()
				.int()
				.optional()
				.describe("replies: message # from the last inbox/history view"),
			peek: z
				.boolean()
				.optional()
				.describe("inbox: do not mark messages as read"),
			limit: z.number().int().optional().describe("history: max messages"),
			with: z
				.string()
				.optional()
				.describe("history: only traffic involving this agent"),
			timeout: z
				.number()
				.optional()
				.describe("wait: seconds to block (default 300)"),
		}),

		async execute(_toolCallId, params, _onUpdate, _ctx, signal) {
			const me = params.as ?? process.env.AGENT_RADIO_AGENT ?? "claude";
			const argv: string[] = [];

			switch (params.op) {
				case "send": {
					if (!params.to || !params.body) {
						throw new Error("send requires 'to' and 'body'");
					}
					argv.push("send", "--from", me, "--to", params.to);
					argv.push("--kind", params.kind ?? "ASK");
					argv.push("--body", params.body);
					for (const f of params.focus ?? []) argv.push("--focus", f);
					if (params.risk) argv.push("--risk", params.risk);
					if (params.priority) argv.push("--priority", params.priority);
					break;
				}
				case "inbox": {
					argv.push("inbox", "--as", me);
					if (params.peek) argv.push("--peek");
					break;
				}
				case "history": {
					argv.push("history", "--as", me);
					if (params.limit != null) argv.push("--limit", String(params.limit));
					if (params.with) argv.push("--with", params.with);
					break;
				}
				case "ack":
				case "done":
				case "decline":
				case "failure": {
					if (params.number == null) {
						throw new Error(
							`${params.op} requires 'number' (from the last inbox/history view)`,
						);
					}
					argv.push(params.op, String(params.number), "--as", me);
					if (params.body) argv.push("--body", params.body);
					break;
				}
				case "team": {
					argv.push("team");
					break;
				}
				case "status": {
					argv.push("status", "--as", me);
					break;
				}
				case "wait": {
					argv.push("wait", "--as", me);
					if (params.timeout != null) {
						argv.push("--timeout", String(params.timeout));
					}
					break;
				}
			}

			const bin = process.env.AGENT_RADIO_BIN ?? "agent-radio";
			const result = await pi.exec(bin, argv, {
				cwd: pi.cwd,
				signal,
			});
			if (result.killed) throw new Error("radio call was cancelled");
			if (result.code !== 0) {
				throw new Error(result.stderr.trim() || `radio exited ${result.code}`);
			}
			const text = result.stdout.trim() || "(empty)";
			return {
				content: [{ type: "text", text }],
				details: { op: params.op, as: me },
			};
		},
	};
};

export default factory;
