import { apiError, jsonResponse, methodNotAllowed } from "./http";
import type { Env, RateLimitRecord } from "./types";

const RATE_RECORD_KEY = "rate";

interface ConsumeRequest {
  limit: number;
  windowSeconds: number;
}

export class RelayRateLimit {
  constructor(
    private readonly state: DurableObjectState,
    _env: Env,
  ) {}

  async fetch(request: Request): Promise<Response> {
    if (request.method !== "POST") {
      return methodNotAllowed(["POST"]);
    }

    let input: ConsumeRequest;
    try {
      input = (await request.json()) as ConsumeRequest;
    } catch {
      return apiError(400, "invalid_rate_limit_request", "The rate limit request is invalid.");
    }
    if (
      !Number.isSafeInteger(input.limit) ||
      input.limit < 1 ||
      input.limit > 100_000 ||
      !Number.isSafeInteger(input.windowSeconds) ||
      input.windowSeconds < 1 ||
      input.windowSeconds > 86_400
    ) {
      return apiError(400, "invalid_rate_limit_request", "The rate limit request is invalid.");
    }

    const now = Date.now();
    let record = await this.state.storage.get<RateLimitRecord>(RATE_RECORD_KEY);
    if (record === undefined || now >= record.resetAt) {
      record = {
        windowStartedAt: now,
        resetAt: now + input.windowSeconds * 1000,
        count: 0,
      };
    }
    record.count += 1;
    await this.state.storage.put(RATE_RECORD_KEY, record);
    await this.state.storage.setAlarm(record.resetAt);

    const allowed = record.count <= input.limit;
    const retryAfter = Math.max(1, Math.ceil((record.resetAt - now) / 1000));
    return jsonResponse({
      allowed,
      remaining: Math.max(0, input.limit - record.count),
      retry_after: retryAfter,
    });
  }

  async alarm(): Promise<void> {
    await this.state.storage.deleteAll();
    await this.state.storage.deleteAlarm();
  }
}
