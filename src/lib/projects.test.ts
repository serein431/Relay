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

describe("项目和会话排序", () => {
  it("项目按最新会话时间倒序排列", () => {
    const projects = groupProjects([
      session({
        id: "older",
        projectKey: "alpha",
        projectName: "Alpha",
        updatedAt: "2026-08-08T10:00:00Z",
      }),
      session({
        id: "newer",
        projectKey: "beta",
        projectName: "Beta",
        updatedAt: "2026-08-09T10:00:00Z",
      }),
    ]);

    expect(projects.map((project) => project.name)).toEqual(["Beta", "Alpha"]);
  });

  it("同一项目的会话按最新时间倒序排列", () => {
    const projects = groupProjects([
      session({
        id: "older",
        projectKey: "relay",
        projectName: "Relay",
        updatedAt: "2026-08-08T10:00:00Z",
      }),
      session({
        id: "newer",
        projectKey: "relay",
        projectName: "Relay",
        updatedAt: "2026-08-09T10:00:00Z",
      }),
    ]);

    expect(projects[0]?.sessions.map((item) => item.id)).toEqual(["newer", "older"]);
  });

  it("更新时间无效时使用创建时间", () => {
    const projects = groupProjects([
      session({
        id: "fallback",
        projectKey: "fallback",
        projectName: "Fallback",
        updatedAt: "invalid",
        createdAt: "2026-08-09T10:00:00Z",
      }),
      session({
        id: "older",
        projectKey: "older",
        projectName: "Older",
        updatedAt: "2026-08-08T10:00:00Z",
      }),
    ]);

    expect(projects[0]?.name).toBe("Fallback");
  });

  it("临时项目使用最新会话的日期名称", () => {
    const older = new Date(2026, 7, 8, 10, 0).toISOString();
    const newer = new Date(2026, 7, 9, 12, 30).toISOString();
    const projects = groupProjects([
      session({
        id: "older",
        projectKey: "temporary",
        createdAt: older,
      }),
      session({
        id: "newer",
        projectKey: "temporary",
        createdAt: newer,
      }),
    ]);

    expect(projects[0]?.name).toBe("8月9日 12:30");
  });
});
