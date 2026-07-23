// Minimal Anthropic /v1/messages shapes — just the fields the agent loop reads.
// We hand-roll these (rather than pull the SDK) because the actual HTTP call is
// made Rust-side by the `llm_complete` proxy; JS only builds the body and walks
// the response content blocks.

export interface TextBlock {
  type: "text";
  text: string;
}

export interface ToolUseBlock {
  type: "tool_use";
  id: string;
  name: string;
  input: Record<string, unknown>;
}

export interface ToolResultBlock {
  type: "tool_result";
  tool_use_id: string;
  content: string;
  is_error?: boolean;
}

/** A block in an assistant response (text or a tool_use request). */
export type ResponseBlock = TextBlock | ToolUseBlock;

/** A block we put back into the conversation (assistant echo or tool results). */
export type RequestBlock = TextBlock | ToolUseBlock | ToolResultBlock;

export interface MessageParam {
  role: "user" | "assistant";
  content: string | RequestBlock[];
}

export interface Usage {
  input_tokens: number;
  output_tokens: number;
}

export type StopReason =
  | "end_turn"
  | "tool_use"
  | "max_tokens"
  | "stop_sequence"
  | "pause_turn"
  | "refusal";

/** A successful /v1/messages response body. */
export interface AnthropicMessage {
  id: string;
  role: "assistant";
  model: string;
  content: ResponseBlock[];
  stop_reason: StopReason | null;
  usage: Usage;
}

/** Provider error body (Anthropic + OpenAI both nest under `error.message`). */
export interface ProviderError {
  error?: { type?: string; message?: string };
}

/** Tool definition as sent to the API. */
export interface ToolDef {
  name: string;
  description: string;
  input_schema: {
    type: "object";
    properties: Record<string, unknown>;
    required?: string[];
  };
}
