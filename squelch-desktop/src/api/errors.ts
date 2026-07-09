// Central error type for the human-door client. Every non-2xx response becomes
// an ApiError; callers switch on `kind` rather than sniffing status codes.

export type ApiErrorKind =
  | "unauthorized" // 401: bad/absent bearer token
  | "forbidden" // 403: no write credential configured (run `squelchd auth --write`)
  | "not_found" // 404: missing OR sealed-hidden (indistinguishable by design)
  | "bad_request" // 400: validation (e.g. missing confirm)
  | "guard_blocked" // 422: outbound send guard matched; see `guardKinds`
  | "server" // 5xx
  | "network" // fetch threw (server down / unreachable)
  | "unknown";

export class ApiError extends Error {
  kind: ApiErrorKind;
  status: number;
  /** Redacted guard kinds parsed from a 422 send response, if any. */
  guardKinds?: string[];

  constructor(
    kind: ApiErrorKind,
    status: number,
    message: string,
    guardKinds?: string[],
  ) {
    super(message);
    this.name = "ApiError";
    this.kind = kind;
    this.status = status;
    this.guardKinds = guardKinds;
  }
}

/**
 * The 422 send-guard message is a human sentence containing the redacted kinds:
 *   "outbound guard blocked send; matched (redacted) kinds: aws_key, jwt.
 *    resend with \"override_guard\": true to send anyway"
 * Parse the comma list between "kinds:" and the trailing period/sentence.
 */
export function parseGuardKinds(message: string): string[] {
  const m = message.match(/kinds:\s*([^.]+)/i);
  if (!m) return [];
  return m[1]
    .split(",")
    .map((s) => s.trim())
    .filter((s) => s.length > 0 && !s.includes(" ")); // drop trailing prose
}

export function kindForStatus(status: number): ApiErrorKind {
  switch (status) {
    case 400:
      return "bad_request";
    case 401:
      return "unauthorized";
    case 403:
      return "forbidden";
    case 404:
      return "not_found";
    case 422:
      return "guard_blocked";
    default:
      return status >= 500 ? "server" : "unknown";
  }
}
