const PRIVATE_KEY_PATTERN = /-----BEGIN (?:RSA |EC |OPENSSH |PGP )?PRIVATE KEY(?: BLOCK)?-----/iu;
const AUTHORIZATION_PATTERN = /authorization["']?\s*[:=]\s*["']?bearer\s+[A-Za-z0-9._~+/=-]{12,}/iu;
const KNOWN_TOKEN_PATTERNS = [
  /(?:^|[^A-Za-z0-9_-])(?:AKIA|ASIA)[A-Z0-9]{16}(?:$|[^A-Za-z0-9])/u,
  /(?:^|[^A-Za-z0-9_-])ghp_[A-Za-z0-9_-]{24,}(?:$|[^A-Za-z0-9_-])/u,
  /(?:^|[^A-Za-z0-9_-])github_pat_[A-Za-z0-9_-]{24,}(?:$|[^A-Za-z0-9_-])/u,
  /(?:^|[^A-Za-z0-9_-])sk-[A-Za-z0-9_-]{20,}(?:$|[^A-Za-z0-9_-])/u,
  /(?:^|[^A-Za-z0-9_-])xox[bp]-[A-Za-z0-9_-]{16,}(?:$|[^A-Za-z0-9_-])/u,
  /(?:^|\s)eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}(?:$|\s)/u,
];
const CONNECTION_PATTERN = /\b(?:postgres(?:ql)?|mysql|mongodb(?:\+srv)?|rediss?|amqps?):\/\//iu;
const SECRET_ASSIGNMENT_PATTERN = /(?:^|\n)\s*(?:export\s+)?[A-Za-z0-9_]*(?:PASSWORD|PASSWD|SECRET|TOKEN|API_KEY|ACCESS_KEY|PRIVATE_KEY|CLIENT_SECRET)\s*=\s*["']?([^\s"',;]{8,})/giu;

function serialized(value: unknown): string {
  if (typeof value === "string") return value;
  try {
    return JSON.stringify(value);
  } catch {
    return "";
  }
}

function hasNonPlaceholderAssignment(text: string): boolean {
  SECRET_ASSIGNMENT_PATTERN.lastIndex = 0;
  for (const match of text.matchAll(SECRET_ASSIGNMENT_PATTERN)) {
    const value = (match[1] ?? "").toLocaleLowerCase("en-US");
    if (
      value &&
      !["example", "sample", "placeholder", "changeme", "your_", "your-", "<secret>", "${"]
        .some((word) => value.includes(word))
    ) {
      return true;
    }
  }
  return false;
}

export function sensitiveHints(value: unknown): string[] {
  const text = serialized(value);
  if (!text) return [];
  const hints: string[] = [];
  if (PRIVATE_KEY_PATTERN.test(text)) hints.push("可能包含私钥正文");
  if (AUTHORIZATION_PATTERN.test(text)) hints.push("可能包含 Authorization 凭据");
  if (KNOWN_TOKEN_PATTERNS.some((pattern) => pattern.test(text))) hints.push("可能包含常见服务令牌");
  if (CONNECTION_PATTERN.test(text)) hints.push("可能包含数据库或消息服务连接串");
  if (hasNonPlaceholderAssignment(text)) hints.push("可能包含密码、密钥或令牌赋值");
  return hints;
}
