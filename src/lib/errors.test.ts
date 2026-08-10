import { describe, expect, it } from "vitest";
import { errorCode, technicalErrorMessage, userErrorMessage } from "./errors";

describe("用户错误说明", () => {
  it("使用错误编号返回明确的中文说明", () => {
    const error = { code: "relaypack_key_invalid", message: "invalid AES key" };
    expect(errorCode(error)).toBe("relaypack_key_invalid");
    expect(userErrorMessage(error)).toBe("文件密码不正确，请确认已经完整复制。");
    expect(technicalErrorMessage(error)).toBe("invalid AES key");
  });

  it("不把未知英文内部错误直接显示给用户", () => {
    expect(userErrorMessage(
      new Error("the native helper returned invalid JSON"),
      "无法读取本机数据。",
    )).toBe("无法读取本机数据。");
  });

  it("保留已经可以直接阅读的中文错误", () => {
    expect(userErrorMessage("连接分享服务超时，请检查网络或代理设置后重试。"))
      .toBe("连接分享服务超时，请检查网络或代理设置后重试。");
  });

  it("将系统权限错误改成可操作的说明", () => {
    expect(userErrorMessage(
      "The request is not allowed by the user agent or the platform in the current context",
    )).toContain("Relay 桌面应用");
  });
});
