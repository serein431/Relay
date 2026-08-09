import { describe, expect, it } from "vitest";
import type { SessionSummary } from "../types";
import { sessionKey, uniqueSessionDisplayTitles } from "./sessions";

function session(overrides: Partial<SessionSummary>): SessionSummary {
  return {
    id: "session-1",
    agent: "codex",
    title: "实现当前框架",
    projectName: "Relay",
    workspace: "main",
    cwd: "/tmp/relay",
    health: "complete",
    warnings: [],
    ...overrides,
  };
}

describe("会话显示身份", () => {
  it("使用来源文件区分同一提供方的会话", () => {
    const first = session({ sourcePath: "/tmp/one.jsonl" });
    const second = session({ sourcePath: "/tmp/two.jsonl" });
    expect(sessionKey(first)).not.toBe(sessionKey(second));
  });

  it("同名会话使用各自摘要生成不同标题", () => {
    const first = session({
      id: "019f8157-bd60-7901-9bbe-d7252374a63c",
      sourcePath: "/tmp/one.jsonl",
      preview: "Task 3 代码质量复审通过，没有 Critical 或 Important 问题。",
    });
    const second = session({
      id: "019f8146-1fff-7361-b8e7-dcafb9ebcec7",
      sourcePath: "/tmp/two.jsonl",
      preview: "✅ compliant 最终复查确认：冻结指纹只允许指定表。",
    });
    const titles = uniqueSessionDisplayTitles([first, second]);
    expect(titles.get(sessionKey(first))).toBe("Task 3 代码质量复审通过");
    expect(titles.get(sessionKey(second))).toBe("compliant 最终复查确认");
    expect(new Set(titles.values())).toHaveLength(2);
  });

  it("摘要仍然相同时追加短会话编号", () => {
    const first = session({ id: "019f8157-a", preview: "检查完成。" });
    const second = session({ id: "019f8146-b", preview: "检查完成。" });
    const titles = uniqueSessionDisplayTitles([first, second]);
    expect(titles.get(sessionKey(first))).toBe("检查完成。");
    expect(titles.get(sessionKey(second))).toContain("019f8146");
  });

  it("UUID 标题使用会话创建时间", () => {
    const createdAt = new Date(2026, 7, 9, 16, 42).toISOString();
    const item = session({
      id: "019fe27e-60f1-7db2-9e22-439818435e13",
      title: "019fe27e-60f1-7db2-9e22-439818435e13",
      createdAt,
    });
    const titles = uniqueSessionDisplayTitles([item]);
    expect(titles.get(sessionKey(item))).toBe("8月9日 16:42");
  });

  it("同一分钟创建的临时会话追加短编号", () => {
    const createdAt = new Date(2026, 7, 9, 16, 42).toISOString();
    const first = session({
      id: "019fe27e-60f1-7db2-9e22-439818435e13",
      title: "未命名会话",
      createdAt,
    });
    const second = session({
      id: "019fe28d-3844-7533-87ac-bfddc549e117",
      title: "019fe28d-3844-7533-87ac-bfddc549e117",
      createdAt,
    });
    const titles = uniqueSessionDisplayTitles([first, second]);
    expect(titles.get(sessionKey(first))).toBe("8月9日 16:42 · 019fe27e");
    expect(titles.get(sessionKey(second))).toBe("8月9日 16:42 · 019fe28d");
  });

  it("正常标题保持不变", () => {
    const item = session({
      title: "检查会话导入结果",
      createdAt: new Date(2026, 7, 9, 16, 42).toISOString(),
    });
    const titles = uniqueSessionDisplayTitles([item]);
    expect(titles.get(sessionKey(item))).toBe("检查会话导入结果");
  });

  it("无标题且无时间时显示临时会话和短编号", () => {
    const item = session({
      id: "019fe27e-60f1-7db2-9e22-439818435e13",
      title: "",
    });
    const titles = uniqueSessionDisplayTitles([item]);
    expect(titles.get(sessionKey(item))).toBe("临时会话 · 019fe27e");
  });
});
