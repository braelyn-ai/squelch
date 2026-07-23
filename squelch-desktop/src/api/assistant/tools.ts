// The two tools the inbox assistant can call. Both are thin wrappers over the
// existing human-door reads (`/client/search`, `/client/thread`) — the assistant
// gets exactly the same sealed-absent, summary-first view the rest of the app
// does. It never sees a full body it wasn't going to show a human anyway, and
// sealed (auth/2FA) messages are structurally absent from both routes.
//
// `runTool` returns a compact JSON string (the tool_result content). Errors are
// returned as `{ error }` JSON with is_error handled by the caller, so the model
// can recover rather than the loop throwing.

import { search, getThread } from "../client";
import type { ClientThreadView, SearchHit } from "../types";
import type { ToolDef } from "./types";

export const ASSISTANT_TOOLS: ToolDef[] = [
  {
    name: "search_mail",
    description:
      "Search the user's mailbox by meaning AND keyword (hybrid recall). " +
      "Returns summaries only — sender, subject, date, a snippet, and a " +
      "thread_id — never full bodies. Auth/2FA messages are excluded. Call " +
      "this first to find the messages relevant to the question.",
    input_schema: {
      type: "object",
      properties: {
        query: {
          type: "string",
          description: "What to look for, phrased naturally.",
        },
        limit: {
          type: "integer",
          description: "Max results to return (default 8, max 20).",
        },
      },
      required: ["query"],
    },
  },
  {
    name: "get_thread",
    description:
      "Read one thread's messages by thread_id (from a search result) when " +
      "the snippet isn't enough to answer. Returns the subject and each " +
      "message's sender, date, and text.",
    input_schema: {
      type: "object",
      properties: {
        thread_id: {
          type: "string",
          description: "The thread_id from a search_mail result.",
        },
      },
      required: ["thread_id"],
    },
  },
];

/** A source the assistant actually consulted — surfaced as an answer citation. */
export interface ToolCitation {
  threadId: string;
  subject: string;
  sender: string;
  date: string;
}

function hitCitation(h: SearchHit): ToolCitation {
  return {
    threadId: h.thread_id,
    subject: h.subject || "(no subject)",
    sender: h.from_name || h.from_addr,
    date: h.received_at,
  };
}

/**
 * Execute one tool call. Pushes any thread the assistant touched into `sinkCites`
 * so the loop can present them as citations. Returns the tool_result content.
 */
export async function runTool(
  name: string,
  input: Record<string, unknown>,
  sinkCites: Map<string, ToolCitation>,
): Promise<{ content: string; isError: boolean }> {
  try {
    if (name === "search_mail") {
      const q = String(input.query ?? "").trim();
      if (!q) return { content: JSON.stringify({ error: "empty query" }), isError: true };
      const limit = Math.min(Number(input.limit ?? 8) || 8, 20);
      const page = await search(q, { limit, mode: "hybrid" });
      const rows = page.items.map((h) => {
        // Remember every hit as a candidate citation, keyed by thread.
        sinkCites.set(h.thread_id, hitCitation(h));
        return {
          thread_id: h.thread_id,
          from: h.from_name || h.from_addr,
          subject: h.subject,
          date: h.received_at,
          snippet: h.snippet,
        };
      });
      return { content: JSON.stringify({ results: rows }), isError: false };
    }

    if (name === "get_thread") {
      const id = String(input.thread_id ?? "").trim();
      if (!id) return { content: JSON.stringify({ error: "missing thread_id" }), isError: true };
      const view: ClientThreadView = await getThread(id);
      // A thread the model chose to open is a strong citation signal.
      const first = view.messages[0];
      if (first) {
        sinkCites.set(view.thread_id, {
          threadId: view.thread_id,
          subject: view.subject || "(no subject)",
          sender: first.from_name || first.from_addr,
          date: first.received_at,
        });
      }
      const messages = view.messages.map((m) => ({
        from: m.from_name || m.from_addr,
        date: m.received_at,
        text: m.content,
      }));
      return {
        content: JSON.stringify({ subject: view.subject, messages }),
        isError: false,
      };
    }

    return { content: JSON.stringify({ error: `unknown tool ${name}` }), isError: true };
  } catch (e) {
    // Surface a compact error to the model so it can adapt (e.g. sealed → 404).
    const msg = e instanceof Error ? e.message : "tool failed";
    return { content: JSON.stringify({ error: msg }), isError: true };
  }
}
