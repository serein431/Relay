import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ConversationViewer, {
  blockContent,
  blockLabel,
  roleLabel,
  toolStatusLabel,
} from "./ConversationViewer";
import type { SessionContentPreview } from "./types";

const preview: SessionContentPreview = {
  schema: "relay.adapter.handoff-preview.v1",
  preview_sha256: "preview-sha",
  source: { agent: "codex", session_id: "session-1", read_only: true },
  session: { title: "检查聊天记录" },
  conversation: {
    messages: [
      {
        id: "message-1",
        role: "user",
        timestamp: "2026-08-09T08:00:00Z",
        blocks: [{ kind: "text", classification: "user_visible", text: "请检查完整记录" }],
      },
      {
        id: "message-2",
        role: "assistant",
        blocks: [{
          kind: "tool_call",
          classification: "user_visible",
          name: "exec_command",
          input: { cmd: "pnpm test" },
        }],
      },
    ],
  },
  diagnostics: { warnings: [], completeness: {} },
};

describe("完整聊天记录", () => {
  it("区分消息角色和工具内容", () => {
    const markup = renderToStaticMarkup(
      <ConversationViewer
        preview={preview}
        loading={false}
        error={null}
        onRetry={() => undefined}
      />,
    );
    expect(markup).toContain("聊天记录");
    expect(markup).toContain("用户");
    expect(markup).toContain("工具调用 · exec_command");
    expect(markup).toContain("pnpm test");
  });

  it("长内容默认提供展开入口", () => {
    const longPreview: SessionContentPreview = {
      ...preview,
      conversation: {
        messages: [{
          id: "long-message",
          role: "assistant",
          blocks: [{
            kind: "text",
            classification: "user_visible",
            text: "长".repeat(3_200),
          }],
        }],
      },
    };
    const markup = renderToStaticMarkup(
      <ConversationViewer
        preview={longPreview}
        loading={false}
        error={null}
        onRetry={() => undefined}
      />,
    );
    expect(markup).toContain("展开完整内容");
  });

  it("提供稳定的文字转换", () => {
    expect(roleLabel("assistant")).toBe("助手");
    expect(blockLabel({ kind: "tool_result", classification: "user_visible" })).toBe("工具结果");
    expect(blockContent({
      kind: "tool_call",
      classification: "user_visible",
      input: { path: "/tmp/relay" },
    })).toContain("/tmp/relay");
    expect(toolStatusLabel("completed")).toBe("已完成");
    expect(toolStatusLabel("error")).toBe("失败");
  });

  it("数千条记录的会话先显示前 200 条，不为尚未显示的消息生成目录项", () => {
    const manyMessages = Array.from({ length: 8_685 }, (_, index) => ({
      id: `message-${index}`,
      role: "user",
      blocks: [{
        kind: "text" as const,
        classification: "user_visible" as const,
        text: `第 ${index + 1} 条用户消息`,
      }],
    }));
    const markup = renderToStaticMarkup(
      <ConversationViewer
        preview={{ ...preview, conversation: { messages: manyMessages } }}
        loading={false}
        error={null}
        onRetry={() => undefined}
      />,
    );

    expect(markup).toContain("再显示 200 条记录");
    expect(markup).toContain("第 200 条用户消息");
    expect(markup).not.toContain("第 201 条用户消息");
  });
});
