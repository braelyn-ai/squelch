// The "ask your inbox" agent loop. Runs entirely client-side with the user's own
// key: build a /v1/messages body, fire it through the Rust `llm_complete` proxy,
// and walk the standard tool-use loop (search_mail / get_thread) until the model
// answers. Non-streaming — each turn is a short complete message we inspect for
// stop_reason; inbox answers are a paragraph, so there's nothing to stream.
//
// Purest expression of the app's guiding principle: ask a question, get a cited
// answer, never open the email yourself.

import { assistantKeyStatus, llmComplete } from "./llm";
import { ASSISTANT_TOOLS, runTool, type ToolCitation } from "./tools";
import { recordAssistantUsage } from "./usage";
import type {
  AnthropicMessage,
  MessageParam,
  ProviderError,
  RequestBlock,
  ResponseBlock,
  ToolResultBlock,
} from "./types";

/** Model options offered in Settings. Cheap-first: Haiku is the default. */
export const ASSISTANT_MODELS = [
  { id: "claude-haiku-4-5", label: "Haiku 4.5 — fast & cheap (recommended)" },
  { id: "claude-opus-4-8", label: "Opus 4.8 — smartest, pricier" },
] as const;

export type AssistantModelId = (typeof ASSISTANT_MODELS)[number]["id"];

const MODEL_KEY = "squelch.assistant.model";
const DEFAULT_MODEL: AssistantModelId = "claude-haiku-4-5";

export function getAssistantModel(): AssistantModelId {
  try {
    const v = localStorage.getItem(MODEL_KEY);
    if (v && ASSISTANT_MODELS.some((m) => m.id === v)) return v as AssistantModelId;
  } catch {
    // fall through to default
  }
  return DEFAULT_MODEL;
}

export function setAssistantModel(id: AssistantModelId): void {
  try {
    localStorage.setItem(MODEL_KEY, id);
  } catch {
    // preference is best-effort
  }
}

/** Safety valve on the tool loop (search + a few thread reads is plenty). */
const MAX_TURNS = 6;
const MAX_TOKENS = 1024;

const SYSTEM = [
  "You are the user's personal inbox assistant, embedded in an app called squelch.",
  "Answer questions about their email using the tools — search_mail first to find",
  "relevant messages, then get_thread only when a snippet isn't enough.",
  "",
  "Rules:",
  "- Be concise and direct. Lead with the answer, then the supporting detail.",
  "- Ground every claim in what the tools returned. If you can't find it, say so",
  "  plainly rather than guessing.",
  "- You are the user's stand-in: the whole point is that they never have to open",
  "  the email themselves. Summarize; don't tell them to go read it.",
  "- Auth codes, 2FA, and password-reset messages are deliberately invisible to",
  "  you. If asked for one, explain it's handled separately in the app, not here.",
  "- Dates in results are RFC3339; refer to them in plain language.",
].join("\n");

export interface AssistantAnswer {
  text: string;
  citations: ToolCitation[];
  usage: { model: string; inputTokens: number; outputTokens: number };
}

export class AssistantError extends Error {}

function textOf(blocks: ResponseBlock[]): string {
  return blocks
    .filter((b): b is Extract<ResponseBlock, { type: "text" }> => b.type === "text")
    .map((b) => b.text)
    .join("")
    .trim();
}

/**
 * Ask a question and return a cited answer. Throws AssistantError on missing key,
 * wrong provider, a provider error, a refusal, or hitting the step limit.
 */
export async function askInbox(question: string): Promise<AssistantAnswer> {
  const status = await assistantKeyStatus();
  if (!status.present) {
    throw new AssistantError("No assistant key set — add one in Settings.");
  }
  if (status.provider !== "anthropic") {
    throw new AssistantError(
      "The assistant currently supports Anthropic keys (sk-ant-…). " +
        "OpenAI support is coming; paste an Anthropic key in Settings for now.",
    );
  }

  const model = getAssistantModel();
  const messages: MessageParam[] = [{ role: "user", content: question.trim() }];

  // Threads surfaced by search vs. actually opened via get_thread. We cite the
  // opened ones when the model drilled in, else the top search hits it saw.
  const cites = new Map<string, ToolCitation>();
  const readIds = new Set<string>();

  let inputTokens = 0;
  let outputTokens = 0;

  for (let turn = 0; turn < MAX_TURNS; turn++) {
    const body = {
      model,
      max_tokens: MAX_TOKENS,
      system: SYSTEM,
      tools: ASSISTANT_TOOLS,
      messages,
    };

    const { status: httpStatus, json } = await llmComplete<
      AnthropicMessage & ProviderError
    >(body);

    if (httpStatus !== 200) {
      const msg = json?.error?.message ?? `assistant request failed (${httpStatus})`;
      throw new AssistantError(msg);
    }

    inputTokens += json.usage?.input_tokens ?? 0;
    outputTokens += json.usage?.output_tokens ?? 0;

    if (json.stop_reason === "refusal") {
      throw new AssistantError("The model declined to answer that.");
    }

    // Echo the assistant turn verbatim (tool_use blocks must be preserved).
    messages.push({ role: "assistant", content: json.content as RequestBlock[] });

    if (json.stop_reason !== "tool_use") {
      const text = textOf(json.content);
      const at = new Date().toISOString();
      recordAssistantUsage(model, inputTokens, outputTokens, at);
      const citations = pickCitations(cites, readIds);
      return {
        text: text || "(the assistant returned no text)",
        citations,
        usage: { model, inputTokens, outputTokens },
      };
    }

    // Run every requested tool, collect all results into ONE user turn.
    const toolResults: ToolResultBlock[] = [];
    for (const block of json.content) {
      if (block.type !== "tool_use") continue;
      if (block.name === "get_thread") {
        const id = String((block.input as { thread_id?: unknown }).thread_id ?? "");
        if (id) readIds.add(id);
      }
      const { content, isError } = await runTool(block.name, block.input, cites);
      toolResults.push({
        type: "tool_result",
        tool_use_id: block.id,
        content,
        is_error: isError,
      });
    }
    messages.push({ role: "user", content: toolResults });
  }

  throw new AssistantError("The assistant took too many steps without answering.");
}

function pickCitations(
  cites: Map<string, ToolCitation>,
  readIds: Set<string>,
): ToolCitation[] {
  const opened = [...readIds]
    .map((id) => cites.get(id))
    .filter((c): c is ToolCitation => c !== undefined);
  if (opened.length > 0) return opened;
  // Nothing opened — cite the first few search hits the model actually saw.
  return [...cites.values()].slice(0, 5);
}
