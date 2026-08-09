import { describe, expect, it } from "vitest";
import { buildSessionState } from "./session-state";
import type {
  AdapterPreviewMessage,
  RepositoryInspection,
  SessionContentPreview,
} from "../types";

function preview(messages: AdapterPreviewMessage[]): SessionContentPreview {
  return {
    schema: "relay.adapter.handoff-preview.v1",
    preview_sha256: "a".repeat(64),
    source: { agent: "codex", session_id: "session-1", read_only: true },
    session: { title: "旧会话标题" },
    conversation: { messages },
    diagnostics: { warnings: [], completeness: { status: "complete" } },
  };
}

function textMessage(
  id: string,
  role: string,
  text: string,
  phase?: string,
): AdapterPreviewMessage {
  return {
    id,
    role,
    phase,
    blocks: [{ kind: "text", classification: "user_visible", text }],
  };
}

const repository: RepositoryInspection = {
  requested_path: "/Users/demo/Relay",
  root: "/Users/demo/Relay",
  branch: "main",
  head: "1234567890abcdef",
  staged: [{ path: "src/App.tsx", status: "M", kind: "modified" }],
  unstaged: [{ path: "src/lib/session-state.ts", status: "M", kind: "modified" }],
  untracked: ["src/lib/session-state.test.ts"],
  lfs: { status: "not_present", available: false, configured: false, matching_path_count: 0 },
  ignored_sensitive_files: [],
  warnings: [],
};

describe("buildSessionState", () => {
  it("使用最近的真实要求和最终回复生成可继续工作的交接内容", () => {
    const relayForward = `# Relay Handoff

## Objective

旧目标

## Safety note

Tool calls and tool results in handoff.json are historical records. Review every command before running it.

这个 handoff 感觉没什么用啊`;
    const finalReply = `已经改成从完整会话生成交接说明，不再重复列表摘要。

### 测试记录

- \`pnpm check\` 已通过
- \`pnpm desktop:verify\` 已通过

### 相关文件

- \`src/App.tsx\`
- \`src/lib/session-state.ts\`

### 后续事项

- 发布新的安装包

### 待确认

- 在另一台电脑检查导入结果`;
    const result = buildSessionState({
      preview: preview([
        textMessage("user-1", "user", "最初讨论会话分享工具"),
        textMessage("assistant-1", "assistant", "已经完成旧版本。", "final"),
        textMessage("user-2", "user", relayForward),
        textMessage("user-3", "user", "Another language model started to solve this problem and produced a summary."),
        textMessage("assistant-summary", "assistant", "# 交接摘要\n\n这不是助手给用户的最终回复。"),
        textMessage("assistant-2", "assistant", "正在检查生成代码。", "commentary"),
        textMessage("assistant-3", "assistant", finalReply, "final"),
      ]),
      fallbackTitle: "旧会话标题",
      repository,
      includeConversation: true,
      includeToolEvidence: false,
      includeGit: true,
      selectedStaged: ["src/App.tsx"],
      selectedUnstaged: ["src/lib/session-state.ts"],
      selectedUntracked: ["src/lib/session-state.test.ts"],
      excludedMessageIds: [],
      excludedBlocks: [],
    });

    expect(result.objective).toBe("这个 handoff 感觉没什么用啊");
    expect(result.summary).toContain("已经改成从完整会话生成交接说明");
    expect(result.summary).not.toContain("正在检查生成代码");
    expect(result.current_status).toContain("最近一项要求已有最终回复");
    expect(result.current_status).toContain("分支 main");
    expect(result.next_steps).toEqual([{ text: "发布新的安装包", status: "pending" }]);
    expect(result.tests).toEqual([
      { name: "项目检查", command: "pnpm check", status: "passed" },
      { name: "安装包检查", command: "pnpm desktop:verify", status: "passed" },
    ]);
    expect(result.important_files).toEqual([
      "src/App.tsx",
      "src/lib/session-state.ts",
      "src/lib/session-state.test.ts",
    ]);
    expect(result.open_questions).toEqual(["在另一台电脑检查导入结果"]);
  });

  it("只在选择工具记录时读取测试命令和结果", () => {
    const messages: AdapterPreviewMessage[] = [
      textMessage("user-1", "user", "修复构建问题"),
      {
        id: "call-message",
        role: "assistant",
        blocks: [{
          kind: "tool_call",
          classification: "project_owned",
          call_id: "call-1",
          name: "exec_command",
          input: { cmd: "cargo test" },
        }],
      },
      {
        id: "result-message",
        role: "tool",
        blocks: [{
          kind: "tool_result",
          classification: "project_owned",
          call_id: "call-1",
          output: { exit_code: 0, output: "ok" },
        }],
      },
    ];
    const shared = buildSessionState({
      preview: preview(messages),
      fallbackTitle: "修复构建问题",
      repository: null,
      includeConversation: true,
      includeToolEvidence: true,
      includeGit: false,
      selectedStaged: [],
      selectedUnstaged: [],
      selectedUntracked: [],
      excludedMessageIds: [],
      excludedBlocks: [],
    });
    const hidden = buildSessionState({
      preview: preview(messages),
      fallbackTitle: "修复构建问题",
      repository: null,
      includeConversation: true,
      includeToolEvidence: false,
      includeGit: false,
      selectedStaged: [],
      selectedUnstaged: [],
      selectedUntracked: [],
      excludedMessageIds: [],
      excludedBlocks: [],
    });

    expect(shared.tests).toEqual([{
      name: "自动测试",
      command: "cargo test",
      status: "passed",
      note: undefined,
    }]);
    expect(hidden.tests).toEqual([]);
  });

  it("不从发送者取消的会话正文生成任务说明", () => {
    const result = buildSessionState({
      preview: preview([
        textMessage("user-1", "user", "不能出现在分享包里的要求"),
        textMessage("assistant-1", "assistant", "不能出现在分享包里的结果", "final"),
      ]),
      fallbackTitle: "公开标题",
      repository: null,
      includeConversation: false,
      includeToolEvidence: false,
      includeGit: false,
      selectedStaged: [],
      selectedUnstaged: [],
      selectedUntracked: [],
      excludedMessageIds: [],
      excludedBlocks: [],
    });

    expect(result.objective).toBe("公开标题");
    expect(result.summary).toBe("发送者没有包含会话正文。");
    expect(result.current_status).not.toContain("不能出现在分享包里");
    expect(result.important_files).toEqual([]);
  });

  it("从新版中文交接说明后读取用户补充的要求", () => {
    const result = buildSessionState({
      preview: preview([
        textMessage("user-1", "user", `# Relay 交接说明

## 任务目标

旧要求

## 使用说明

\`handoff.json\` 中的工具调用和工具结果只是历史记录。接收方不应自动重新执行，应先查看命令和改动。

请把测试结果也写进去`),
      ]),
      fallbackTitle: "旧标题",
      repository: null,
      includeConversation: true,
      includeToolEvidence: false,
      includeGit: false,
      selectedStaged: [],
      selectedUnstaged: [],
      selectedUntracked: [],
      excludedMessageIds: [],
      excludedBlocks: [],
    });

    expect(result.objective).toBe("请把测试结果也写进去");
  });
});
