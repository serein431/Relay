import { useEffect, useMemo, useState } from "react";
import { copyText } from "./lib/clipboard";
import type {
  AdapterPreviewBlock,
  AdapterPreviewMessage,
  SessionContentPreview,
} from "./types";

type ConversationViewerProps = {
  preview: SessionContentPreview | null;
  loading: boolean;
  error: string | null;
  onRetry: () => void;
  onNotice?: (message: string) => void;
};

type MessageFilter = "all" | "conversation" | "tools";

const LONG_CONTENT_THRESHOLD = 3_000;
const COLLAPSED_CONTENT_LENGTH = 1_500;
const INITIAL_MESSAGE_LIMIT = 200;
const MESSAGE_PAGE_SIZE = 200;

function normalizeSearchText(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

export function roleLabel(role: string): string {
  switch (role.toLocaleLowerCase("zh-CN")) {
    case "user": return "用户";
    case "assistant": return "助手";
    case "tool": return "工具";
    case "system": return "系统";
    case "developer": return "开发说明";
    default: return role || "未知来源";
  }
}

export function blockLabel(block: AdapterPreviewBlock): string {
  switch (block.kind) {
    case "text": return "文字";
    case "tool_call": return block.name ? `工具调用 · ${block.name}` : "工具调用";
    case "tool_result": return "工具结果";
    case "source_context": return block.native_type ? `项目指令 · ${block.native_type}` : "项目指令";
    case "asset_ref": return block.native_type ? `附件 · ${block.native_type}` : "附件";
    case "unsupported": return block.native_type ? `未识别内容 · ${block.native_type}` : "未识别内容";
    default: return block.kind || "其他内容";
  }
}

function stringify(value: unknown): string {
  if (typeof value === "string") return value;
  if (value === undefined || value === null) return "没有可显示的内容。";
  try {
    return JSON.stringify(value, null, 2);
  } catch {
    return String(value);
  }
}

export function blockContent(block: AdapterPreviewBlock): string {
  if (block.text !== undefined) return block.text;
  if (block.kind === "tool_call") return stringify(block.input);
  if (block.kind === "tool_result") return stringify(block.output);
  return stringify(block.source);
}

function isToolBlock(block: AdapterPreviewBlock): boolean {
  return block.kind === "tool_call" || block.kind === "tool_result";
}

function blocksForFilter(
  message: AdapterPreviewMessage,
  filter: MessageFilter,
): AdapterPreviewBlock[] {
  if (filter === "tools") return message.blocks.filter(isToolBlock);
  if (filter === "conversation") return message.blocks.filter((block) => !isToolBlock(block));
  return message.blocks;
}

function messageSearchText(
  message: AdapterPreviewMessage,
  blocks: AdapterPreviewBlock[],
): string {
  return [
    roleLabel(message.role),
    message.phase,
    ...blocks.flatMap((block) => [
      blockLabel(block),
      block.name,
      block.call_id,
      blockContent(block),
    ]),
  ]
    .filter(Boolean)
    .join("\n")
    .toLocaleLowerCase("zh-CN");
}

function formatTimestamp(value?: string): string {
  if (!value) return "时间未知";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return date.toLocaleString("zh-CN", {
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
}

function messageCopyText(message: AdapterPreviewMessage): string {
  return message.blocks
    .map((block) => blockContent(block).trim())
    .filter(Boolean)
    .join("\n\n");
}

function tocPreview(message: AdapterPreviewMessage): string {
  const value = message.blocks
    .filter((block) => block.kind === "text")
    .map(blockContent)
    .join(" ");
  const normalized = normalizeSearchText(value);
  if (!normalized) return "用户消息";
  const characters = Array.from(normalized);
  return characters.length > 72 ? `${characters.slice(0, 72).join("")}…` : normalized;
}

function HighlightedText({ text, query }: { text: string; query: string }) {
  const needle = query.trim();
  if (!needle) return text;
  const lowerText = text.toLocaleLowerCase("zh-CN");
  const lowerNeedle = needle.toLocaleLowerCase("zh-CN");
  const parts: Array<{ text: string; match: boolean }> = [];
  let cursor = 0;
  while (cursor < text.length) {
    const index = lowerText.indexOf(lowerNeedle, cursor);
    if (index < 0) {
      parts.push({ text: text.slice(cursor), match: false });
      break;
    }
    if (index > cursor) parts.push({ text: text.slice(cursor, index), match: false });
    parts.push({ text: text.slice(index, index + needle.length), match: true });
    cursor = index + needle.length;
  }
  return parts.map((part, index) => part.match
    ? <mark key={index}>{part.text}</mark>
    : <span key={index}>{part.text}</span>);
}

function ConversationBlock({
  block,
  blockKey,
  query,
  expanded,
  onToggle,
}: {
  block: AdapterPreviewBlock;
  blockKey: string;
  query: string;
  expanded: boolean;
  onToggle: (key: string) => void;
}) {
  const content = blockContent(block);
  const isLong = content.length > LONG_CONTENT_THRESHOLD;
  const searchMatch = query.trim() !== "" && content
    .toLocaleLowerCase("zh-CN")
    .includes(query.trim().toLocaleLowerCase("zh-CN"));
  const collapsed = isLong && !expanded && !searchMatch;
  const displayContent = collapsed
    ? `${content.slice(0, COLLAPSED_CONTENT_LENGTH)}…`
    : content;

  return (
    <section className={`conversation-block kind-${block.kind}`}>
      <header>
        <strong>{blockLabel(block)}</strong>
        <span>
          {block.status ? <b>{block.status}</b> : null}
          {block.call_id ? <code title={block.call_id}>调用编号 {block.call_id}</code> : null}
        </span>
      </header>
      <pre className={isToolBlock(block) ? "is-technical" : undefined}>
        <HighlightedText text={displayContent} query={query} />
      </pre>
      {isLong && !searchMatch ? (
        <button
          className="conversation-expand"
          type="button"
          aria-expanded={expanded}
          onClick={() => onToggle(blockKey)}
        >
          {expanded ? "收起长内容" : `展开完整内容（约 ${Math.ceil(content.length / 1_000)} 千字）`}
        </button>
      ) : null}
    </section>
  );
}

export default function ConversationViewer({
  preview,
  loading,
  error,
  onRetry,
  onNotice,
}: ConversationViewerProps) {
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<MessageFilter>("all");
  const [visibleLimit, setVisibleLimit] = useState(INITIAL_MESSAGE_LIMIT);
  const [expandedBlocks, setExpandedBlocks] = useState<Set<string>>(() => new Set());
  const [copiedMessageId, setCopiedMessageId] = useState<string | null>(null);

  const messages = preview?.conversation.messages ?? [];
  const needle = query.trim().toLocaleLowerCase("zh-CN");
  const filtered = useMemo(() => messages.flatMap((message, sourceIndex) => {
    const blocks = blocksForFilter(message, filter);
    if (blocks.length === 0) return [];
    if (needle && !messageSearchText(message, blocks).includes(needle)) return [];
    return [{ message, blocks, sourceIndex }];
  }), [filter, messages, needle]);
  const visible = filtered.slice(0, visibleLimit);
  const toolBlockCount = messages.reduce((count, message) =>
    count + message.blocks.filter(isToolBlock).length, 0);
  const tocItems = filtered.filter(({ message }) => message.role === "user");

  useEffect(() => {
    setVisibleLimit(INITIAL_MESSAGE_LIMIT);
  }, [filter, needle, preview?.source.session_id]);

  useEffect(() => {
    setExpandedBlocks(new Set());
    setCopiedMessageId(null);
  }, [preview?.source.session_id]);

  const toggleBlock = (key: string) => {
    setExpandedBlocks((current) => {
      const next = new Set(current);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  const copyMessage = async (message: AdapterPreviewMessage) => {
    try {
      await copyText(messageCopyText(message));
      setCopiedMessageId(message.id);
      window.setTimeout(() => setCopiedMessageId(null), 1_800);
      onNotice?.("消息内容已复制。");
    } catch {
      onNotice?.("复制失败，请手动选择文字复制。");
    }
  };

  if (loading && !preview) {
    return (
      <div className="conversation-loading" aria-live="polite">
        <div className="conversation-skeleton"><i /><b /><span /></div>
        <div className="conversation-skeleton"><i /><b /><span /></div>
        <div className="conversation-skeleton"><i /><b /><span /></div>
        <p>正在读取完整聊天记录</p>
      </div>
    );
  }

  if (error && !preview) {
    return (
      <div className="conversation-state is-error">
        <strong>聊天记录读取失败</strong>
        <p>{error}</p>
        <button type="button" onClick={onRetry}>重新读取</button>
      </div>
    );
  }

  if (!preview) {
    return (
      <div className="conversation-state">
        <strong>没有可显示的聊天记录</strong>
        <p>选择本机会话后，Relay 会在这里读取消息正文和工具记录。</p>
      </div>
    );
  }

  return (
    <section className="conversation-viewer" aria-label="完整聊天记录">
      <header className="conversation-toolbar">
        <div className="conversation-summary">
          <strong>聊天记录</strong>
          <span>{messages.length} 条消息</span>
          <span>{toolBlockCount} 项工具记录</span>
        </div>
        <label className="conversation-search">
          <span className="sr-only">搜索聊天记录</span>
          <input
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder="搜索消息、工具名或正文"
          />
        </label>
        <div className="conversation-filters" aria-label="记录类型">
          {([
            ["all", "全部"],
            ["conversation", "对话"],
            ["tools", "工具记录"],
          ] as const).map(([value, label]) => (
            <button
              key={value}
              type="button"
              className={filter === value ? "is-active" : ""}
              aria-pressed={filter === value}
              onClick={() => setFilter(value)}
            >
              {label}
            </button>
          ))}
        </div>
      </header>

      <div className="conversation-layout">
        <div className="conversation-scroll">
          {visible.length === 0 ? (
            <div className="conversation-state compact">
              <strong>没有匹配的记录</strong>
              <p>请更换关键词或记录类型。</p>
            </div>
          ) : (
            <div className="conversation-message-list">
              {visible.map(({ message, blocks, sourceIndex }) => (
                <article
                  id={`conversation-message-${sourceIndex}`}
                  className={`conversation-message role-${message.role}`}
                  key={`${message.id}-${sourceIndex}`}
                >
                  <header className="conversation-message-header">
                    <span className="conversation-role-mark">{roleLabel(message.role).slice(0, 1)}</span>
                    <div>
                      <strong>{roleLabel(message.role)}</strong>
                      <time>{formatTimestamp(message.timestamp)}</time>
                    </div>
                    <button type="button" onClick={() => void copyMessage(message)}>
                      {copiedMessageId === message.id ? "已复制" : "复制"}
                    </button>
                  </header>
                  <div className="conversation-block-list">
                    {blocks.map((block, blockIndex) => {
                      const key = `${message.id}\u0000${blockIndex}`;
                      return (
                        <ConversationBlock
                          key={key}
                          block={block}
                          blockKey={key}
                          query={query}
                          expanded={expandedBlocks.has(key)}
                          onToggle={toggleBlock}
                        />
                      );
                    })}
                  </div>
                </article>
              ))}
              {visible.length < filtered.length ? (
                <button
                  className="conversation-show-more"
                  type="button"
                  onClick={() => setVisibleLimit((value) => value + MESSAGE_PAGE_SIZE)}
                >
                  再显示 {Math.min(MESSAGE_PAGE_SIZE, filtered.length - visible.length)} 条记录
                </button>
              ) : null}
            </div>
          )}
        </div>

        {tocItems.length > 2 ? (
          <aside className="conversation-toc" aria-label="用户消息目录">
            <strong>用户消息目录</strong>
            <div>
              {tocItems.map(({ message, sourceIndex }, index) => (
                <button
                  type="button"
                  key={`${message.id}-${sourceIndex}`}
                  onClick={() => document
                    .getElementById(`conversation-message-${sourceIndex}`)
                    ?.scrollIntoView({ behavior: "smooth", block: "start" })}
                >
                  <span>{index + 1}</span>
                  <b>{tocPreview(message)}</b>
                </button>
              ))}
            </div>
          </aside>
        ) : null}
      </div>
    </section>
  );
}
