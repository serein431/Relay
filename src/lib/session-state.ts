import type {
  AdapterPreviewBlock,
  AdapterPreviewMessage,
  ExcludedContentBlock,
  ExportRelaypackRequest,
  RepositoryInspection,
  SessionContentPreview,
} from "../types";

export type GeneratedSessionState = NonNullable<ExportRelaypackRequest["session_state"]>;

type BuildSessionStateOptions = {
  preview: SessionContentPreview;
  fallbackTitle: string;
  repository: RepositoryInspection | null;
  includeConversation: boolean;
  includeToolEvidence: boolean;
  includeGit: boolean;
  selectedStaged: string[];
  selectedUnstaged: string[];
  selectedUntracked: string[];
  excludedMessageIds: string[];
  excludedBlocks: ExcludedContentBlock[];
};

type IndexedText = {
  index: number;
  message: AdapterPreviewMessage;
  text: string;
};

type GeneratedTest = NonNullable<GeneratedSessionState["tests"]>[number];

const SHORT_ACKNOWLEDGEMENT = /^(?:好|好的|可以|行|继续|继续吧|接着来|知道了|明白了|没问题|完成了|好了|登录好了|ok|okay|yes|done)[。.!！]?$/i;
const SYNTHETIC_CONTEXT = /^(?:another language model started to solve this problem|#\s*交接摘要(?:\s|$)|#\s*relay(?:\s+handoff|\s+交接说明)(?:\s|$))/i;
const TEST_COMMAND = /^(?:pnpm\s+(?:run\s+)?(?:test|check|build|lint|typecheck|desktop:[\w:-]+)|npm\s+(?:run\s+)?(?:test|check|build|lint|typecheck)|yarn\s+(?:test|check|build|lint|typecheck)|bun\s+(?:test|run\s+(?:test|check|build|lint|typecheck))|cargo\s+(?:test|check|clippy|fmt|build)\b|go\s+(?:test|vet)\b|pytest\b|python(?:3)?\s+-m\s+pytest\b|bash\s+scripts\/[^\s]+(?:verify|check|test)[^\s]*|git\s+diff\s+--check\b|make\s+(?:test|check|build|lint)\b)/i;
const ROOT_FILE = /^(?:README(?:\.[\w.-]+)?|AGENTS\.md|CLAUDE\.md|package\.json|pnpm-lock\.yaml|Cargo\.(?:toml|lock)|go\.(?:mod|sum)|tsconfig(?:\.[\w.-]+)?\.json)$/i;

function normalizeWhitespace(value: string): string {
  return value.replace(/\r\n?/g, "\n").replace(/[ \t]+/g, " ").trim();
}

function truncateText(value: string, limit: number): string {
  const characters = Array.from(value.trim());
  if (characters.length <= limit) return characters.join("");
  const shortened = characters.slice(0, limit).join("");
  const boundary = Math.max(
    shortened.lastIndexOf("。"),
    shortened.lastIndexOf("！"),
    shortened.lastIndexOf("？"),
    shortened.lastIndexOf("."),
    shortened.lastIndexOf("\n"),
  );
  const result = boundary >= Math.floor(limit * 0.55)
    ? shortened.slice(0, boundary + 1)
    : shortened;
  return `${result.trim()}…`;
}

function cleanUserText(value: string): string {
  let text = value
    .replace(/<in-app-browser-context\b[\s\S]*?<\/in-app-browser-context>/gi, "\n")
    .replace(/<appshot\b[\s\S]*?<\/appshot>/gi, "\n")
    .replace(/<oai-mem-citation>[\s\S]*?<\/oai-mem-citation>/gi, "\n");

  const requestMarker = text.toLocaleLowerCase("en-US").lastIndexOf("## my request:");
  if (requestMarker >= 0) {
    text = text.slice(requestMarker + "## my request:".length);
  }

  if (/# Relay (?:Handoff|交接说明)/.test(text)) {
    const endings = [
      "Review every command before running it.",
      "接收方不应自动重新执行，应先查看命令和改动。",
    ];
    const ending = endings
      .map((value) => ({ value, index: text.lastIndexOf(value) }))
      .sort((left, right) => right.index - left.index)[0];
    text = ending && ending.index >= 0
      ? text.slice(ending.index + ending.value.length)
      : "";
  }

  text = normalizeWhitespace(text);
  if (SYNTHETIC_CONTEXT.test(text)) return "";
  return text;
}

function cleanAssistantText(value: string): string {
  return normalizeWhitespace(value
    .replace(/<oai-mem-citation>[\s\S]*?<\/oai-mem-citation>/gi, "\n")
    .replace(/^::[\w-]+\{[^\n]*\}\s*$/gm, "")
    .replace(/```[\s\S]*?```/g, "（代码片段已省略）"));
}

function selectedBlock(
  messageId: string,
  blockIndex: number,
  excludedMessages: Set<string>,
  excludedBlocks: Set<string>,
): boolean {
  return !excludedMessages.has(messageId)
    && !excludedBlocks.has(`${messageId}:${blockIndex}`);
}

function messageText(
  message: AdapterPreviewMessage,
  excludedMessages: Set<string>,
  excludedBlocks: Set<string>,
): string {
  if (excludedMessages.has(message.id)) return "";
  return message.blocks
    .flatMap((block, blockIndex) => selectedBlock(
      message.id,
      blockIndex,
      excludedMessages,
      excludedBlocks,
    ) && block.kind === "text" && block.text ? [block.text] : [])
    .join("\n\n")
    .trim();
}

function isFinalAssistantMessage(message: AdapterPreviewMessage): boolean {
  if (message.role.toLocaleLowerCase("en-US") !== "assistant") return false;
  const phase = message.phase?.toLocaleLowerCase("en-US");
  return phase !== "commentary" && phase !== "analysis";
}

function isProgressMessage(message: AdapterPreviewMessage): boolean {
  return message.role.toLocaleLowerCase("en-US") === "assistant"
    && message.phase?.toLocaleLowerCase("en-US") === "commentary";
}

function isSyntheticAssistantText(value: string): boolean {
  return SYNTHETIC_CONTEXT.test(normalizeWhitespace(value));
}

function meaningfulUserMessages(messages: IndexedText[]): IndexedText[] {
  return messages.filter(({ message, text }) => {
    if (message.role.toLocaleLowerCase("en-US") !== "user") return false;
    const cleaned = cleanUserText(text);
    return cleaned.length >= 4 && !SHORT_ACKNOWLEDGEMENT.test(cleaned);
  }).map((entry) => ({ ...entry, text: cleanUserText(entry.text) }));
}

function summarizeFinalReply(value: string): string {
  const cleaned = cleanAssistantText(value);
  if (!cleaned) return "";
  const lines = cleaned.split("\n");
  const kept: string[] = [];
  let skippedSection = false;
  for (const line of lines) {
    const heading = line.replace(/^\s*#{1,6}\s*/, "").replace(/^\*\*(.*?)\*\*$/, "$1").trim();
    if (/^(?:测试|测试记录|相关文件|文件|后续事项|下一步|待办|注意事项|限制与注意事项|待确认|待确认问题|未决问题|使用说明)$/i.test(heading)) {
      skippedSection = true;
      continue;
    }
    if (/^\s*#{1,6}\s+/.test(line)) skippedSection = false;
    if (!skippedSection) kept.push(line);
  }
  return truncateText(kept.join("\n").replace(/\n{3,}/g, "\n\n"), 1_600);
}

function sectionItems(value: string, sectionNames: RegExp): string[] {
  const lines = value.replace(/\r\n?/g, "\n").split("\n");
  const items: string[] = [];
  let active = false;
  for (const sourceLine of lines) {
    const line = sourceLine.trim();
    const heading = line
      .replace(/^#{1,6}\s*/, "")
      .replace(/^\*\*(.*?)\*\*:?$/, "$1")
      .replace(/[：:]$/, "")
      .trim();
    const looksLikeHeading = /^#{1,6}\s+/.test(line) || /^\*\*.*\*\*:?$/.test(line);
    if (sectionNames.test(heading)) {
      active = true;
      continue;
    }
    if (active && looksLikeHeading) break;
    if (!active || !line) continue;
    const item = line
      .replace(/^[-*+]\s+/, "")
      .replace(/^\d+[.)、]\s*/, "")
      .replace(/^\[[ xX]\]\s*/, "")
      .trim();
    if (!item || /^(?:无|没有|暂无|不需要|无需|none|n\/a)[。.!！]?$/i.test(item)) continue;
    if (/^如果.{0,30}(?:可以|愿意)/.test(item)) continue;
    items.push(truncateText(item, 300));
    if (items.length >= 8) break;
  }
  return items;
}

function nextStepStatus(text: string): string {
  if (/已完成|已经完成|done/i.test(text)) return "done";
  if (/进行中|正在|in progress/i.test(text)) return "in_progress";
  if (/等待|依赖|无法继续|blocked/i.test(text)) return "blocked";
  return "pending";
}

function extractNextSteps(value: string): NonNullable<GeneratedSessionState["next_steps"]> {
  const items = sectionItems(value, /^(?:下一步|后续事项|待办|尚未完成|仍需处理|接下来|需要继续)$/i);
  if (items.length === 0) {
    const explicit = value.match(/(?:仍需|还需要|尚未完成|待处理)[：:，,]?\s*([^。！？\n]{4,220})/g) ?? [];
    for (const match of explicit) {
      const item = match.trim();
      if (!/无需|不需要|没有/.test(item)) items.push(truncateText(item, 300));
      if (items.length >= 8) break;
    }
  }
  return [...new Set(items)].map((text) => ({ text, status: nextStepStatus(text) }));
}

function extractOpenQuestions(value: string): string[] {
  return [...new Set(sectionItems(value, /^(?:待确认|未决问题|需要确认|尚待确认|问题)$/i))];
}

function normalizeCommand(value: string): string {
  return value
    .trim()
    .replace(/^\$\s*/, "")
    .replace(/[。；;]+$/, "")
    .replace(/\s+/g, " ")
    .slice(0, 300);
}

function isTestCommand(value: string): boolean {
  return TEST_COMMAND.test(normalizeCommand(value));
}

function testName(command: string): string {
  if (/desktop:build|cargo\s+build|(?:pnpm|npm|yarn|bun)\s+(?:run\s+)?build/i.test(command)) {
    return "应用构建";
  }
  if (/desktop:verify|verify-macos-bundle/i.test(command)) return "安装包检查";
  if (/\b(?:test|pytest)\b/i.test(command)) return "自动测试";
  if (/clippy|lint|vet\b/i.test(command)) return "静态检查";
  if (/fmt|format/i.test(command)) return "格式检查";
  return "项目检查";
}

function statusNearText(value: string, index: number): string {
  const lineStart = value.lastIndexOf("\n", index) + 1;
  const nextBreak = value.indexOf("\n", index);
  const lineEnd = nextBreak >= 0 ? nextBreak : value.length;
  const nearby = value.slice(lineStart, lineEnd);
  if (/失败|未通过|报错|failed|failure|error/i.test(nearby)) return "failed";
  if (/未运行|没有运行|not run/i.test(nearby)) return "not_run";
  if (/通过|成功|已完成|全部完成|passed|succeeded|success/i.test(nearby)) return "passed";
  return "unknown";
}

function commandsFromText(value: string): GeneratedTest[] {
  const tests: GeneratedTest[] = [];
  const codeSpans = /`([^`\r\n]{2,300})`/g;
  for (const match of value.matchAll(codeSpans)) {
    const command = normalizeCommand(match[1]);
    if (!isTestCommand(command)) continue;
    const status = statusNearText(value, match.index ?? 0);
    tests.push({
      name: testName(command),
      command,
      status,
      note: status === "unknown" ? "最终回复提到了该命令，但没有写明结果。" : undefined,
    });
  }
  return tests;
}

function collectCommandStrings(value: unknown, depth = 0): string[] {
  if (depth > 5 || value === null || value === undefined) return [];
  if (typeof value === "string") {
    const result: string[] = [];
    const direct = normalizeCommand(value);
    if (isTestCommand(direct)) result.push(direct);
    try {
      const parsed = JSON.parse(value) as unknown;
      result.push(...collectCommandStrings(parsed, depth + 1));
    } catch {
      for (const match of value.matchAll(/(?:cmd|command)\s*[:=]\s*["'`]([^"'`\n]{2,300})["'`]/gi)) {
        const command = normalizeCommand(match[1]);
        if (isTestCommand(command)) result.push(command);
      }
    }
    return result;
  }
  if (Array.isArray(value)) {
    return value.flatMap((item) => collectCommandStrings(item, depth + 1));
  }
  if (typeof value === "object") {
    const result: string[] = [];
    for (const [key, item] of Object.entries(value)) {
      if (/^(?:cmd|command|script)$/i.test(key) && typeof item === "string") {
        const command = normalizeCommand(item);
        if (isTestCommand(command)) result.push(command);
      } else {
        result.push(...collectCommandStrings(item, depth + 1));
      }
    }
    return result;
  }
  return [];
}

function toolResultStatus(block?: AdapterPreviewBlock): string {
  if (!block) return "unknown";
  if (block.is_error === true || /fail|error/i.test(block.status ?? "")) return "failed";
  const output = block.output;
  if (typeof output === "object" && output !== null && "exit_code" in output) {
    const exitCode = (output as { exit_code?: unknown }).exit_code;
    if (typeof exitCode === "number") return exitCode === 0 ? "passed" : "failed";
  }
  const serialized = typeof output === "string" ? output : JSON.stringify(output ?? "");
  if (/exit[_ ]code["']?\s*[:=]\s*[1-9]\d*|process exited with code [1-9]\d*|script failed/i.test(serialized)) {
    return "failed";
  }
  if (/exit[_ ]code["']?\s*[:=]\s*0|process exited with code 0|script completed|tests? passed/i.test(serialized)) {
    return "passed";
  }
  return "unknown";
}

function commandsFromTools(
  messages: AdapterPreviewMessage[],
  excludedMessages: Set<string>,
  excludedBlocks: Set<string>,
): GeneratedTest[] {
  const results = new Map<string, AdapterPreviewBlock>();
  for (const message of messages) {
    message.blocks.forEach((block, blockIndex) => {
      if (!selectedBlock(message.id, blockIndex, excludedMessages, excludedBlocks)) return;
      if (block.kind === "tool_result" && block.call_id) results.set(block.call_id, block);
    });
  }

  const tests: GeneratedTest[] = [];
  for (const message of messages) {
    message.blocks.forEach((block, blockIndex) => {
      if (!selectedBlock(message.id, blockIndex, excludedMessages, excludedBlocks)) return;
      if (block.kind !== "tool_call") return;
      for (const command of collectCommandStrings(block.input)) {
        const status = toolResultStatus(block.call_id ? results.get(block.call_id) : undefined);
        tests.push({
          name: testName(command),
          command,
          status,
          note: status === "unknown" ? "会话记录中没有找到明确的命令结果。" : undefined,
        });
      }
    });
  }
  return tests;
}

function mergeTests(tests: GeneratedTest[]): GeneratedTest[] {
  const rank: Record<string, number> = { failed: 4, passed: 3, not_run: 2, unknown: 1 };
  const merged = new Map<string, GeneratedTest>();
  for (const test of tests) {
    const key = test.command?.toLocaleLowerCase("en-US") ?? test.name.toLocaleLowerCase("zh-CN");
    const current = merged.get(key);
    if (!current || (rank[test.status ?? "unknown"] ?? 0) > (rank[current.status ?? "unknown"] ?? 0)) {
      merged.set(key, test);
    }
  }
  return [...merged.values()].slice(0, 12);
}

function relativeRepositoryPath(value: string, repositoryRoot?: string): string | null {
  let candidate = value.trim().replace(/^repo:\/\//, "").replace(/[),.;，。；]+$/, "");
  candidate = candidate.replace(/:\d+(?::\d+)?$/, "");
  if (repositoryRoot && candidate.startsWith(`${repositoryRoot}/`)) {
    candidate = candidate.slice(repositoryRoot.length + 1);
  }
  if (!candidate || candidate.startsWith("/") || candidate.includes("\\") || candidate.includes("\0")) return null;
  if (candidate.includes(":") || /(?:^|\/)\.\.?(?:\/|$)/.test(candidate)) return null;
  if (candidate.split("/").some((part) => !part || part === "." || part === "..")) return null;
  if (!candidate.includes("/") && !ROOT_FILE.test(candidate)) return null;
  if (!/\.[A-Za-z0-9_-]{1,12}$/.test(candidate) && !ROOT_FILE.test(candidate)) return null;
  return candidate;
}

function filesFromText(value: string, repositoryRoot?: string): string[] {
  const files: string[] = [];
  for (const match of value.matchAll(/`([^`\r\n]{2,500})`/g)) {
    const file = relativeRepositoryPath(match[1], repositoryRoot);
    if (file) files.push(file);
  }
  return files;
}

function repositoryStatus(
  repository: RepositoryInspection | null,
  includeGit: boolean,
  selectedCount: number,
): string {
  if (!includeGit || !repository) return "本次分享不包含 Git 内容。";
  const parts: string[] = [];
  if (repository.branch) parts.push(`分支 ${repository.branch}`);
  if (repository.head) parts.push(`提交 ${repository.head.slice(0, 12)}`);
  if (selectedCount > 0) parts.push(`选择了 ${selectedCount} 个未提交文件`);
  else parts.push("没有选择未提交文件");
  return `Git：${parts.join("，")}。`;
}

export function buildSessionState({
  preview,
  fallbackTitle,
  repository,
  includeConversation,
  includeToolEvidence,
  includeGit,
  selectedStaged,
  selectedUnstaged,
  selectedUntracked,
  excludedMessageIds,
  excludedBlocks,
}: BuildSessionStateOptions): GeneratedSessionState {
  const excludedMessages = new Set(excludedMessageIds);
  const excludedBlockKeys = new Set(
    excludedBlocks.map((block) => `${block.message_id}:${block.block_index}`),
  );
  const indexedText = preview.conversation.messages.map((message, index) => ({
    index,
    message,
    text: messageText(message, excludedMessages, excludedBlockKeys),
  })).filter(({ text }) => Boolean(text));

  const userMessages = includeConversation ? meaningfulUserMessages(indexedText) : [];
  const objectiveMessage = userMessages.at(-1);
  const objective = truncateText(objectiveMessage?.text || fallbackTitle, 600);
  const objectiveIndex = objectiveMessage?.index ?? -1;

  const finalReplies = includeConversation
    ? indexedText.filter(({ index, message, text }) => index > objectiveIndex
      && isFinalAssistantMessage(message)
      && !isSyntheticAssistantText(text))
    : [];
  const previousFinalReplies = includeConversation
    ? indexedText.filter(({ index, message, text }) => index <= objectiveIndex
      && isFinalAssistantMessage(message)
      && !isSyntheticAssistantText(text))
    : [];
  const latestFinal = finalReplies.at(-1);
  const previousFinal = previousFinalReplies.at(-1);
  const progress = includeConversation
    ? indexedText.filter(({ index, message }) => index > objectiveIndex && isProgressMessage(message)).at(-1)
    : undefined;

  let summary = "发送者没有包含会话正文。";
  if (latestFinal) {
    summary = summarizeFinalReply(latestFinal.text) || "最近一项要求已经有最终回复。";
  } else if (previousFinal) {
    const previous = summarizeFinalReply(previousFinal.text);
    summary = previous ? `本轮尚未形成最终回复。此前最近完成的内容：\n\n${previous}` : "本轮尚未形成最终回复。";
  } else if (includeConversation) {
    summary = "会话中还没有可用于说明完成结果的最终回复。";
  }

  const selectedFiles = [...selectedStaged, ...selectedUnstaged, ...selectedUntracked];
  const gitStatus = repositoryStatus(repository, includeGit, selectedFiles.length);
  let currentStatus: string;
  if (!includeConversation) {
    currentStatus = `无法从未包含的会话正文判断任务进度。${gitStatus}`;
  } else if (latestFinal) {
    const hasUnfinishedWork = /尚未|还没有|未完成|失败|无法/.test(latestFinal.text);
    currentStatus = `${hasUnfinishedWork ? "最近一项要求已有回复，但回复中仍记录了未完成或失败的事项。" : "最近一项要求已有最终回复。"}${gitStatus}`;
  } else if (progress) {
    currentStatus = `最近一项要求尚未出现最终回复。最近进度：${truncateText(cleanAssistantText(progress.text), 420)} ${gitStatus}`;
  } else {
    currentStatus = `最近一项要求尚未出现最终回复。${gitStatus}`;
  }

  const sourceForDetails = latestFinal?.text ?? "";
  const visibleTests = includeConversation ? commandsFromText(sourceForDetails) : [];
  const toolTests = includeToolEvidence
    ? commandsFromTools(preview.conversation.messages, excludedMessages, excludedBlockKeys)
    : [];
  const inferredFiles = includeConversation
    ? filesFromText(sourceForDetails, repository?.root)
    : [];
  const importantFiles = [...new Set([
    ...(includeGit ? selectedFiles : []),
    ...inferredFiles,
  ].map((file) => relativeRepositoryPath(file, repository?.root)).filter((file): file is string => Boolean(file)))].slice(0, 24);

  const constraints = ["工具调用和工具结果只是历史记录，不会自动执行。"];
  if (includeGit && repository) constraints.push("分享包包含发送者选择的 Git 内容，接收后应先查看改动再继续。 ");
  if (excludedMessageIds.length > 0 || excludedBlocks.length > 0) {
    constraints.push(`发送者排除了 ${excludedMessageIds.length} 条消息和 ${excludedBlocks.length} 个内容块。`);
  }
  if (!includeConversation) constraints.push("发送者没有包含会话正文。");

  return {
    objective,
    summary,
    current_status: currentStatus.trim(),
    next_steps: includeConversation ? extractNextSteps(sourceForDetails) : [],
    tests: mergeTests([...toolTests, ...visibleTests]),
    important_files: importantFiles,
    constraints: constraints.map((item) => item.trim()),
    open_questions: includeConversation ? extractOpenQuestions(sourceForDetails) : [],
  };
}
