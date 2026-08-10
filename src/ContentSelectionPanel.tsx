import { useEffect, useMemo, useState } from "react";
import { sensitiveHints } from "./lib/sensitive";
import type {
  AdapterPreviewBlock,
  AdapterPreviewMessage,
  ExcludedContentBlock,
  SessionContentPreview,
} from "./types";

type ContentSelectionPanelProps = {
  preview: SessionContentPreview;
  options?: ContentSelectionOptions;
  excludedMessageIds: string[];
  excludedBlocks: ExcludedContentBlock[];
  onExcludedMessageIdsChange: (value: string[]) => void;
  onExcludedBlocksChange: (value: ExcludedContentBlock[]) => void;
  onSensitiveCountChange?: (value: number) => void;
  onSelectionSummaryChange?: (value: ContentSelectionSummary) => void;
};

export type ContentSelectionOptions = {
  conversation: boolean;
  toolEvidence: boolean;
  projectInstructions: boolean;
};

export type ContentSelectionSummary = {
  messages: number;
  blocks: number;
  toolBlocks: number;
  instructionBlocks: number;
  sensitive: number;
};

const allContentOptions: ContentSelectionOptions = {
  conversation: true,
  toolEvidence: true,
  projectInstructions: true,
};

const PAGE_SIZE = 160;

function blockKey(messageId: string, blockIndex: number): string {
  return `${messageId}\u0000${blockIndex}`;
}

export function updateExcludedBlocks(
  messages: AdapterPreviewMessage[],
  excludedBlocks: ExcludedContentBlock[],
  messageId: string,
  blockIndex: number,
  included: boolean,
): ExcludedContentBlock[] {
  const message = messages.find((candidate) => candidate.id === messageId);
  const target = message?.blocks[blockIndex];
  if (!message || !target) return excludedBlocks;

  const affected = new Set<string>([blockKey(message.id, blockIndex)]);
  if (
    target.call_id &&
    (target.kind === "tool_call" || target.kind === "tool_result")
  ) {
    for (const candidateMessage of messages) {
      candidateMessage.blocks.forEach((candidate, candidateIndex) => {
        if (
          candidate.call_id === target.call_id &&
          (candidate.kind === "tool_call" || candidate.kind === "tool_result")
        ) {
          affected.add(blockKey(candidateMessage.id, candidateIndex));
        }
      });
    }
  }

  const next = new Map(
    excludedBlocks.map((item) => [blockKey(item.message_id, item.block_index), item]),
  );
  for (const key of affected) {
    if (included) {
      next.delete(key);
    } else {
      const separator = key.lastIndexOf("\u0000");
      next.set(key, {
        message_id: key.slice(0, separator),
        block_index: Number(key.slice(separator + 1)),
      });
    }
  }
  return [...next.values()];
}

export function updateMessageSelection(
  messages: AdapterPreviewMessage[],
  excludedMessageIds: string[],
  excludedBlocks: ExcludedContentBlock[],
  messageId: string,
  included: boolean,
): { excludedMessageIds: string[]; excludedBlocks: ExcludedContentBlock[] } {
  const message = messages.find((candidate) => candidate.id === messageId);
  if (!message) return { excludedMessageIds, excludedBlocks };
  const nextMessages = new Set(excludedMessageIds);
  if (included) nextMessages.delete(message.id);
  else nextMessages.add(message.id);

  const affected = new Set<string>();
  const relatedCallIds = new Set<string>();
  message.blocks.forEach((block, blockIndex) => {
    affected.add(blockKey(message.id, blockIndex));
    if (
      block.call_id &&
      (block.kind === "tool_call" || block.kind === "tool_result")
    ) {
      relatedCallIds.add(block.call_id);
    }
  });
  if (relatedCallIds.size > 0) {
    for (const candidateMessage of messages) {
      candidateMessage.blocks.forEach((block, blockIndex) => {
        if (
          block.call_id &&
          relatedCallIds.has(block.call_id) &&
          (block.kind === "tool_call" || block.kind === "tool_result")
        ) {
          affected.add(blockKey(candidateMessage.id, blockIndex));
        }
      });
    }
  }

  const nextBlocks = new Map(
    excludedBlocks.map((item) => [blockKey(item.message_id, item.block_index), item]),
  );
  for (const key of affected) {
    if (included) {
      nextBlocks.delete(key);
      continue;
    }
    const separator = key.lastIndexOf("\u0000");
    nextBlocks.set(key, {
      message_id: key.slice(0, separator),
      block_index: Number(key.slice(separator + 1)),
    });
  }
  return {
    excludedMessageIds: [...nextMessages],
    excludedBlocks: [...nextBlocks.values()],
  };
}

function roleLabel(role: string): string {
  switch (role) {
    case "user": return "用户";
    case "assistant": return "助手";
    case "system": return "系统";
    case "developer": return "开发说明";
    case "tool": return "工具";
    default: return role || "未知";
  }
}

function blockLabel(block: AdapterPreviewBlock): string {
  switch (block.kind) {
    case "text": return "文字";
    case "tool_call": return `工具调用${block.name ? ` · ${block.name}` : ""}`;
    case "tool_result": return "工具调用结果";
    case "asset_ref": return `附件${block.native_type ? ` · ${block.native_type}` : ""}`;
    case "source_context": return `项目说明${block.native_type ? ` · ${block.native_type}` : ""}`;
    case "context_compacted": return "上下文已自动压缩";
    case "unsupported": return `未识别内容${block.native_type ? ` · ${block.native_type}` : ""}`;
    default: return block.kind || "未知内容";
  }
}

function blockValue(block: AdapterPreviewBlock): unknown {
  if (block.text) return block.text;
  if (block.kind === "tool_call") return block.input;
  if (block.kind === "tool_result") return block.output;
  if (block.kind === "context_compacted") return "原会话在这里整理过上下文；压缩前的隐藏状态不会包含在分享内容中。";
  return block.source;
}

function blockEnabled(
  block: AdapterPreviewBlock,
  options: ContentSelectionOptions,
): boolean {
  if (block.kind === "tool_call" || block.kind === "tool_result") {
    return options.toolEvidence;
  }
  if (block.kind === "source_context") return options.projectInstructions;
  return options.conversation;
}

function previewText(value: unknown): string {
  if (typeof value === "string") return value.trim().slice(0, 900);
  if (value === undefined) return "没有可展示的正文。";
  try {
    const text = JSON.stringify(value, null, 2);
    return text.length > 900 ? `${text.slice(0, 900)}…` : text;
  } catch {
    return "内容无法预览。";
  }
}

function messageSearchText(
  message: AdapterPreviewMessage,
  options: ContentSelectionOptions,
): string {
  return [
    message.role,
    message.phase,
    ...message.blocks
      .filter((block) => blockEnabled(block, options))
      .flatMap((block) => [block.kind, block.name, block.call_id, previewText(blockValue(block))]),
  ]
    .filter(Boolean)
    .join("\n")
    .toLocaleLowerCase("zh-CN");
}

function formatTimestamp(value?: string): string {
  if (!value) return "时间未知";
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString("zh-CN");
}

export default function ContentSelectionPanel({
  preview,
  options = allContentOptions,
  excludedMessageIds,
  excludedBlocks,
  onExcludedMessageIdsChange,
  onExcludedBlocksChange,
  onSensitiveCountChange,
  onSelectionSummaryChange,
}: ContentSelectionPanelProps) {
  const [query, setQuery] = useState("");
  const [visibleLimit, setVisibleLimit] = useState(PAGE_SIZE);
  const messages = preview.conversation.messages;
  const excludedMessages = useMemo(() => new Set(excludedMessageIds), [excludedMessageIds]);
  const excludedBlockKeys = useMemo(
    () => new Set(excludedBlocks.map((item) => blockKey(item.message_id, item.block_index))),
    [excludedBlocks],
  );
  const shareableMessages = useMemo(
    () => messages.filter((message) =>
      message.blocks.some((block) => blockEnabled(block, options))),
    [messages, options],
  );
  const needle = query.trim().toLocaleLowerCase("zh-CN");
  const filtered = useMemo(
    () => needle
      ? shareableMessages.filter((message) => messageSearchText(message, options).includes(needle))
      : shareableMessages,
    [needle, options, shareableMessages],
  );
  const visible = filtered.slice(0, visibleLimit);

  const selectionSummary = useMemo<ContentSelectionSummary>(() => {
    const summary = {
      messages: 0,
      blocks: 0,
      toolBlocks: 0,
      instructionBlocks: 0,
      sensitive: 0,
    };
    for (const message of shareableMessages) {
      if (excludedMessages.has(message.id)) continue;
      let messageIncluded = false;
      message.blocks.forEach((block, index) => {
        if (!blockEnabled(block, options)) return;
        if (excludedBlockKeys.has(blockKey(message.id, index))) return;
        messageIncluded = true;
        summary.blocks += 1;
        if (block.kind === "tool_call" || block.kind === "tool_result") {
          summary.toolBlocks += 1;
        }
        if (block.kind === "source_context") summary.instructionBlocks += 1;
        if (sensitiveHints(blockValue(block)).length > 0) summary.sensitive += 1;
      });
      if (messageIncluded) summary.messages += 1;
    }
    return summary;
  }, [excludedBlockKeys, excludedMessages, options, shareableMessages]);
  const sensitiveSelectedCount = selectionSummary.sensitive;

  useEffect(() => {
    onSensitiveCountChange?.(sensitiveSelectedCount);
  }, [onSensitiveCountChange, sensitiveSelectedCount]);

  const setMessageIncluded = (message: AdapterPreviewMessage, included: boolean) => {
    const next = updateMessageSelection(
      messages,
      excludedMessageIds,
      excludedBlocks,
      message.id,
      included,
    );
    onExcludedMessageIdsChange(next.excludedMessageIds);
    onExcludedBlocksChange(next.excludedBlocks);
  };

  const setBlockIncluded = (
    message: AdapterPreviewMessage,
    blockIndex: number,
    included: boolean,
  ) => {
    onExcludedBlocksChange(updateExcludedBlocks(
      messages,
      excludedBlocks,
      message.id,
      blockIndex,
      included,
    ));
  };

  useEffect(() => {
    onSelectionSummaryChange?.(selectionSummary);
  }, [
    onSelectionSummaryChange,
    selectionSummary,
  ]);

  return (
    <section className="content-selection">
      <header className="content-selection-heading">
        <div>
          <h3>会话内容</h3>
          <p>默认保留已选择的内容。需要时可排除单条消息或工具记录。</p>
        </div>
        <div className="content-selection-counts">
          <strong>{selectionSummary.messages} 条消息</strong>
          <span>共 {selectionSummary.blocks} 项内容</span>
        </div>
      </header>

      <details className="content-selection-details">
        <summary>
          <span>逐项检查会话内容</span>
          <small>可取消单条消息、工具调用或结果</small>
        </summary>

        <div className="content-selection-toolbar">
          <input
            value={query}
            onChange={(event) => {
              setQuery(event.target.value);
              setVisibleLimit(PAGE_SIZE);
            }}
            placeholder="搜索消息、工具名或正文"
          />
          <button
            type="button"
            onClick={() => {
              onExcludedMessageIdsChange([]);
              onExcludedBlocksChange([]);
            }}
          >
            全部保留
          </button>
          <button
            type="button"
            onClick={() => onExcludedMessageIdsChange(
              shareableMessages.map((message) => message.id),
            )}
          >
            取消全部 {shareableMessages.length} 条
          </button>
        </div>

        {sensitiveSelectedCount > 0 ? (
          <div className="content-sensitive-summary">
            仍有 {sensitiveSelectedCount} 个已选内容可能含敏感信息。请逐项检查，或取消对应内容。
          </div>
        ) : null}

        <div className="content-message-list">
          {visible.map((message) => {
          const messageExcluded = excludedMessages.has(message.id);
          const includedBlocks = messageExcluded
            ? 0
            : message.blocks.filter((block, index) =>
              blockEnabled(block, options) &&
              !excludedBlockKeys.has(blockKey(message.id, index))).length;
          const shareableBlockCount = message.blocks.filter((block) =>
            blockEnabled(block, options)).length;
          const messageIncluded = includedBlocks > 0;
          const partial = messageIncluded && includedBlocks < shareableBlockCount;
          return (
            <article className={`content-message${messageIncluded ? "" : " is-excluded"}`} key={message.id}>
              <header>
                <button
                  type="button"
                  className={`content-message-check${partial ? " is-partial" : ""}`}
                  role="checkbox"
                  aria-checked={partial ? "mixed" : messageIncluded}
                  aria-label={`${messageIncluded ? "排除" : "保留"}这条${roleLabel(message.role)}消息`}
                  onClick={() => setMessageIncluded(message, !messageIncluded)}
                >
                  <i />
                </button>
                <div>
                  <strong>{roleLabel(message.role)}</strong>
                  <span>{formatTimestamp(message.timestamp)}</span>
                </div>
                <code>{includedBlocks} / {shareableBlockCount}</code>
              </header>

              <div className="content-block-list">
                {message.blocks.map((block, blockIndex) => {
                  if (!blockEnabled(block, options)) return null;
                  const excluded = messageExcluded || excludedBlockKeys.has(blockKey(message.id, blockIndex));
                  const hints = sensitiveHints(blockValue(block));
                  return (
                    <label className={`content-block${excluded ? " is-excluded" : ""}`} key={`${message.id}-${blockIndex}`}>
                      <input
                        type="checkbox"
                        checked={!excluded}
                        disabled={messageExcluded}
                        onChange={() => setBlockIncluded(message, blockIndex, excluded)}
                      />
                      <span className="content-block-body">
                        <span className="content-block-topline">
                          <strong>{blockLabel(block)}</strong>
                        </span>
                        <pre>{previewText(blockValue(block))}</pre>
                        {hints.length > 0 ? (
                          <span className="content-sensitive-hints">
                            {hints.map((hint) => <b key={hint}>{hint}</b>)}
                          </span>
                        ) : null}
                      </span>
                    </label>
                  );
                })}
              </div>
            </article>
          );
          })}
        </div>

        {visible.length < filtered.length ? (
          <button
            type="button"
            className="content-show-more"
            onClick={() => setVisibleLimit((value) => value + PAGE_SIZE)}
          >
            再显示 {Math.min(PAGE_SIZE, filtered.length - visible.length)} 条
          </button>
        ) : null}
        {filtered.length === 0 ? <p className="content-selection-empty">没有匹配的会话内容。</p> : null}
      </details>
    </section>
  );
}
