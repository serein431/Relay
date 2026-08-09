import { describe, expect, it } from "vitest";
import type { SessionSummary } from "../types";
import { groupProjects } from "./projects";

function session(overrides: Partial<SessionSummary>): SessionSummary {
  return {
    id: "session-1",
    agent: "codex",
    title: "临时会话",
    projectName: "Unknown project",
    workspace: "main",
    cwd: "",
    health: "complete",
    warnings: [],
    ...overrides,
  };
}

describe("项目名称", () => {
  it("没有项目名称时使用会话日期", () => {
    const createdAt = new Date(2026, 7, 9, 16, 42).toISOString();
    const projects = groupProjects([session({ createdAt })]);
    expect(projects[0]?.name).toBe("8月9日 16:42");
  });

  it("已有项目名称保持不变", () => {
    const projects = groupProjects([session({ projectName: "Session Share" })]);
    expect(projects[0]?.name).toBe("Session Share");
  });
});
