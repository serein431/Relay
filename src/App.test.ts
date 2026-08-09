import { describe, expect, it } from "vitest";
import { initialOptions, nativeErrorCode, shareOptionCopy } from "./App";
import { groupProjects } from "./lib/projects";
import type { SessionSummary } from "./types";

describe("分享选项说明", () => {
  it("五类内容都有明确的范围说明", () => {
    expect(Object.keys(shareOptionCopy)).toHaveLength(5);
    expect(shareOptionCopy.conversation.detail).toContain("不包含系统提示、模型私有推理");
    expect(shareOptionCopy.toolEvidence.detail).toContain("不会重新执行");
    expect(shareOptionCopy.gitState.description).toContain("未跟踪文件");
    expect(shareOptionCopy.projectInstructions.description).toContain("AGENTS.md");
    expect(shareOptionCopy.environment.detail).toContain("不包含环境变量值、密钥");
  });

  it("默认关闭更容易带出机器信息的内容", () => {
    expect(initialOptions).toMatchObject({
      conversation: true,
      toolEvidence: false,
      gitState: true,
      projectInstructions: true,
      environment: false,
    });
  });

  it("项目列表使用仓库根目录，同时保留会话自己的工作目录", () => {
    const sessions: SessionSummary[] = [{
      id: "session-1",
      agent: "codex",
      title: "检查工作树",
      projectKey: "git:relay",
      projectName: "Relay",
      projectRoot: "/Users/demo/Relay",
      workspace: "feature",
      cwd: "/Users/demo/Relay-feature",
      health: "complete",
      warnings: [],
    }];
    const [project] = groupProjects(sessions);
    expect(project.path).toBe("/Users/demo/Relay");
    expect(project.sessions[0].cwd).toBe("/Users/demo/Relay-feature");
  });

  it("能识别普通文件夹不是 Git 仓库的本机错误", () => {
    expect(nativeErrorCode({
      code: "not_a_git_repository",
      message: "not inside a Git worktree",
    })).toBe("not_a_git_repository");
    expect(nativeErrorCode(new Error("network failed"))).toBeUndefined();
  });
});
