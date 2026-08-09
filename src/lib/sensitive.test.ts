import { describe, expect, it } from "vitest";
import { sensitiveHints } from "./sensitive";

describe("sensitiveHints", () => {
  it("reports safe labels without returning the matched secret", () => {
    const secret = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";
    const hints = sensitiveHints({
      authorization: "Bearer this-is-a-long-authorization-value",
      token: secret,
      database: "postgres://relay:password@example.test/relay",
    });
    expect(hints).toContain("可能包含 Authorization 凭据");
    expect(hints).toContain("可能包含常见服务令牌");
    expect(hints).toContain("可能包含数据库或消息服务连接串");
    expect(JSON.stringify(hints)).not.toContain(secret);
    expect(JSON.stringify(hints)).not.toContain("password");
  });

  it("ignores common documentation placeholders", () => {
    expect(sensitiveHints("API_KEY=your_api_key_here\nTOKEN=changeme")).toEqual([]);
  });

  it("detects private key material and secret assignments", () => {
    const hints = sensitiveHints(
      "-----BEGIN OPENSSH PRIVATE KEY-----\nCLIENT_SECRET=abcdefghijklmnopqrstuvwxyz",
    );
    expect(hints).toContain("可能包含私钥正文");
    expect(hints).toContain("可能包含密码、密钥或令牌赋值");
  });
});
