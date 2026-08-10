import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import ContentSelectionPanel, {
  updateExcludedBlocks,
  updateMessageSelection,
} from "./ContentSelectionPanel";
import type { AdapterPreviewMessage, SessionContentPreview } from "./types";

const messages: AdapterPreviewMessage[] = [
  {
    id: "message.user",
    role: "user",
    blocks: [{ kind: "text", classification: "user_visible", text: "请检查这个项目" }],
  },
  {
    id: "message.call",
    role: "assistant",
    blocks: [{
      kind: "tool_call",
      classification: "user_visible",
      call_id: "call-1",
      name: "shell",
      input: { command: "echo ok" },
    }],
  },
  {
    id: "message.result",
    role: "tool",
    blocks: [{
      kind: "tool_result",
      classification: "user_visible",
      call_id: "call-1",
      output: "CLIENT_SECRET=abcdefghijklmnopqrstuvwxyz",
    }],
  },
];

const preview: SessionContentPreview = {
  schema: "relay.adapter.handoff-preview.v1",
  preview_sha256: "0".repeat(64),
  source: { agent: "codex", session_id: "session-1", read_only: true },
  session: { title: "选择内容" },
  conversation: { messages },
  diagnostics: { warnings: [], completeness: {} },
};

describe("ContentSelectionPanel", () => {
  it("renders messages, tools and safe sensitive-data labels", () => {
    const markup = renderToStaticMarkup(
      <ContentSelectionPanel
        preview={preview}
        excludedMessageIds={[]}
        excludedBlocks={[]}
        onExcludedMessageIdsChange={() => undefined}
        onExcludedBlocksChange={() => undefined}
      />,
    );
    expect(markup).toContain("逐项检查会话内容");
    expect(markup).toContain("工具调用 · shell");
    expect(markup).toContain("工具调用结果");
    expect(markup).toContain("可能包含密码、密钥或令牌赋值");
    expect(markup.match(/可能包含密码、密钥或令牌赋值/g)).toHaveLength(1);
  });

  it("只显示上一步已经开启的内容类型", () => {
    const markup = renderToStaticMarkup(
      <ContentSelectionPanel
        preview={preview}
        options={{ conversation: true, toolEvidence: false, projectInstructions: true }}
        excludedMessageIds={[]}
        excludedBlocks={[]}
        onExcludedMessageIdsChange={() => undefined}
        onExcludedBlocksChange={() => undefined}
      />,
    );
    expect(markup).toContain("请检查这个项目");
    expect(markup).not.toContain("工具调用 · shell");
    expect(markup).not.toContain("CLIENT_SECRET");
    expect(markup).toContain("1 条消息");
    expect(markup).not.toContain("可能含敏感信息");
  });

  it("toggles a tool call and its matching result together", () => {
    const excluded = updateExcludedBlocks(messages, [], "message.call", 0, false);
    expect(excluded).toEqual(expect.arrayContaining([
      { message_id: "message.call", block_index: 0 },
      { message_id: "message.result", block_index: 0 },
    ]));
    expect(excluded).toHaveLength(2);

    const restored = updateExcludedBlocks(messages, excluded, "message.result", 0, true);
    expect(restored).toEqual([]);
  });

  it("toggling a whole message also toggles its related tool result", () => {
    const removed = updateMessageSelection(messages, [], [], "message.call", false);
    expect(removed.excludedMessageIds).toEqual(["message.call"]);
    expect(removed.excludedBlocks).toEqual(expect.arrayContaining([
      { message_id: "message.call", block_index: 0 },
      { message_id: "message.result", block_index: 0 },
    ]));

    const restored = updateMessageSelection(
      messages,
      removed.excludedMessageIds,
      removed.excludedBlocks,
      "message.call",
      true,
    );
    expect(restored.excludedMessageIds).toEqual([]);
    expect(restored.excludedBlocks).toEqual([]);
  });

  it("数千条记录只先显示前 160 条供检查", () => {
    const manyMessages: AdapterPreviewMessage[] = Array.from(
      { length: 8_685 },
      (_, index) => ({
        id: `large-message-${index}`,
        role: index % 2 === 0 ? "user" : "assistant",
        blocks: [{
          kind: "text",
          classification: "user_visible",
          text: `第 ${index + 1} 条记录`,
        }],
      }),
    );
    const markup = renderToStaticMarkup(
      <ContentSelectionPanel
        preview={{ ...preview, conversation: { messages: manyMessages } }}
        excludedMessageIds={[]}
        excludedBlocks={[]}
        onExcludedMessageIdsChange={() => undefined}
        onExcludedBlocksChange={() => undefined}
      />,
    );

    expect(markup).toContain("取消全部 8685 条");
    expect(markup).toContain("再显示 160 条");
    expect(markup).toContain("第 160 条记录");
    expect(markup).not.toContain("第 161 条记录");
  });
});
