import type { Env } from "./types";

export interface ApiErrorBody {
  error: {
    code: string;
    message: string;
  };
}

export function jsonResponse(value: unknown, status = 200, headers?: HeadersInit): Response {
  const responseHeaders = new Headers(headers);
  responseHeaders.set("Content-Type", "application/json; charset=utf-8");
  return new Response(JSON.stringify(value), { status, headers: responseHeaders });
}

export function apiError(status: number, code: string, message: string, headers?: HeadersInit): Response {
  const body: ApiErrorBody = { error: { code, message } };
  return jsonResponse(body, status, headers);
}

export function methodNotAllowed(methods: string[]): Response {
  return apiError(405, "method_not_allowed", "This method is not allowed for the resource.", {
    Allow: methods.join(", "),
  });
}

export function parsePositiveInteger(value: string | undefined, fallback: number): number {
  if (value === undefined || value.trim() === "") {
    return fallback;
  }
  if (!/^[1-9][0-9]*$/u.test(value)) {
    return fallback;
  }
  const parsed = Number(value);
  return Number.isSafeInteger(parsed) ? parsed : fallback;
}

export function requestOriginAllowed(request: Request, env: Env): boolean {
  const origin = request.headers.get("Origin");
  if (origin === null) {
    return true;
  }
  return allowedCorsOrigin(origin, env) !== null;
}

export function corsPreflight(request: Request, env: Env): Response {
  const origin = request.headers.get("Origin");
  if (origin === null) {
    return new Response(null, { status: 204 });
  }
  const allowed = allowedCorsOrigin(origin, env);
  if (allowed === null) {
    return apiError(403, "origin_not_allowed", "This browser origin is not allowed.");
  }
  return new Response(null, {
    status: 204,
    headers: {
      "Access-Control-Allow-Origin": allowed,
      "Access-Control-Allow-Methods": "GET, HEAD, POST, PUT, DELETE, OPTIONS",
      "Access-Control-Allow-Headers":
        "authorization, content-type, x-relay-ciphertext-sha256",
      "Access-Control-Max-Age": "600",
      Vary: "Origin",
    },
  });
}

export function finalizeResponse(response: Response, request: Request, env: Env): Response {
  const headers = new Headers(response.headers);
  headers.set("Cache-Control", "no-store, max-age=0");
  headers.set("Pragma", "no-cache");
  headers.set("Referrer-Policy", "no-referrer");
  headers.set("X-Content-Type-Options", "nosniff");
  headers.set("X-Frame-Options", "DENY");
  headers.set("X-Robots-Tag", "noindex, nofollow, noarchive");
  headers.set(
    "Permissions-Policy",
    "camera=(), microphone=(), geolocation=(), payment=(), usb=(), interest-cohort=()",
  );
  if (!headers.has("Content-Security-Policy")) {
    headers.set(
      "Content-Security-Policy",
      "default-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
    );
  }
  if (new URL(request.url).protocol === "https:") {
    headers.set("Strict-Transport-Security", "max-age=31536000; includeSubDomains");
  }

  const origin = request.headers.get("Origin");
  if (origin !== null) {
    const allowed = allowedCorsOrigin(origin, env);
    if (allowed !== null) {
      headers.set("Access-Control-Allow-Origin", allowed);
      headers.set(
        "Access-Control-Expose-Headers",
        "content-length, digest, etag, retry-after, x-relay-ciphertext-sha256",
      );
      appendVary(headers, "Origin");
    }
  }

  return new Response(response.body, {
    status: response.status,
    statusText: response.statusText,
    headers,
  });
}

export function bearerToken(request: Request): string | null {
  const authorization = request.headers.get("Authorization");
  if (authorization === null) {
    return null;
  }
  const match = /^Bearer ([A-Za-z0-9_-]{43})$/u.exec(authorization);
  return match?.[1] ?? null;
}

function allowedCorsOrigin(origin: string, env: Env): string | null {
  const configured = (env.ALLOWED_ORIGINS ?? "")
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
  if (configured.includes("*")) {
    return "*";
  }
  return configured.includes(origin) ? origin : null;
}

function appendVary(headers: Headers, value: string): void {
  const existing = headers.get("Vary");
  if (existing === null || existing.trim() === "") {
    headers.set("Vary", value);
    return;
  }
  const values = existing.split(",").map((item) => item.trim().toLowerCase());
  if (!values.includes(value.toLowerCase())) {
    headers.set("Vary", `${existing}, ${value}`);
  }
}
