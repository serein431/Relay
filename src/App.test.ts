import { describe, expect, it } from "vitest";
import {
  initialOptions,
  issueDescription,
  nativeErrorCode,
  previewUpdateMessage,
  reconcilePreviewSelection,
  shareOptionCopy,
  shareOptionsSummary,
} from "./App";
import { groupProjects } from "./lib/projects";
import type { SessionContentPreview, SessionSummary } from "./types";

describe("分享选项说明", () => {
  it("五类内容都使用简短的用户说明", () => {
    expect(Object.keys(shareOptionCopy)).toHaveLength(5);
    expect(shareOptionCopy.conversation.description).toContain("可见消息");
    expect(shareOptionCopy.toolEvidence.description).toContain("只供接收者查看");
    expect(shareOptionCopy.gitState.description).toContain("新文件");
    expect(shareOptionCopy.projectInstructions.description).toContain("AGENTS.md");
    expect(shareOptionCopy.environment.description).toContain("操作系统");
  });

  it("默认包含会话、工具记录和项目说明，不包含代码改动", () => {
    expect(initialOptions).toMatchObject({
      conversation: true,
      toolEvidence: true,
      gitState: false,
      projectInstructions: true,
      environment: false,
    });
    expect(shareOptionsSummary(initialOptions)).toBe(
      "当前发送：聊天记录、工具记录、项目说明。",
    );
  });

  it("发送内容摘要随选项变化", () => {
    expect(shareOptionsSummary({
      conversation: false,
      toolEvidence: false,
      gitState: false,
      projectInstructions: false,
      environment: true,
    })).toBe("当前发送：设备信息。");
    expect(shareOptionsSummary({
      conversation: false,
      toolEvidence: false,
      gitState: false,
      projectInstructions: false,
      environment: false,
    })).toBe("当前没有选择发送内容。");
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

  it("本机会话错误使用正式中文，内部说明不进入主文案", () => {
    expect(issueDescription({
      stage: "adapter_health",
      code: "adapter_timeout",
      message: "process deadline exceeded after 30000ms",
      severity: "error",
    })).toBe("本机会话较多或文件较大，读取时间超过限制。请稍后重试。");
    expect(issueDescription({
      stage: "discover_sessions",
      code: "future_error",
      message: "unknown internal message",
      severity: "error",
    })).not.toContain("unknown internal message");
  });

  it("会话更新后保留仍然有效的内容选择", () => {
    const previous = {
      source: { agent: "codex", session_id: "session-1", read_only: true },
      conversation: {
        messages: [
          { id: "message-1", role: "user", blocks: [{ kind: "text", classification: "user_visible", text: "旧内容" }] },
          { id: "message-2", role: "assistant", blocks: [{ kind: "text", classification: "user_visible", text: "旧回复" }] },
        ],
      },
    } as SessionContentPreview;
    const latest = {
      ...previous,
      conversation: {
        messages: [
          previous.conversation.messages[0],
          { id: "message-3", role: "assistant", blocks: [{ kind: "text", classification: "user_visible", text: "新回复" }] },
        ],
      },
    } as SessionContentPreview;

    expect(reconcilePreviewSelection(
      latest,
      ["message-1", "message-2"],
      [
        { message_id: "message-1", block_index: 0 },
        { message_id: "message-2", block_index: 0 },
        { message_id: "message-1", block_index: 9 },
      ],
    )).toEqual({
      excludedMessageIds: ["message-1"],
      excludedBlocks: [{ message_id: "message-1", block_index: 0 }],
    });
    expect(previewUpdateMessage(previous, latest)).toContain("已新增 1 条记录");
  });
});
