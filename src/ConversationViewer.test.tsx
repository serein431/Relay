import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ConversationViewer, { blockContent, blockLabel, roleLabel } from "./ConversationViewer";
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
  });
});
