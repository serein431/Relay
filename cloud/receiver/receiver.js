import { decodeBrowserRelaypack } from "../src/browser-relaypack.ts";

const state = {
  ciphertext: null,
  handoff: null,
  markdown: "",
  records: [],
  filter: "all",
  query: "",
};

const elements = {
  loading: document.querySelector("#loading"),
  error: document.querySelector("#error"),
  errorTitle: document.querySelector("#error-title"),
  errorMessage: document.querySelector("#error-message"),
  viewer: document.querySelector("#viewer"),
  title: document.querySelector("#share-title"),
  project: document.querySelector("#share-project"),
  agent: document.querySelector("#share-agent"),
  expiry: document.querySelector("#share-expiry"),
  messageCount: document.querySelector("#message-count"),
  toolCount: document.querySelector("#tool-count"),
  search: document.querySelector("#message-search"),
  messages: document.querySelector("#message-list"),
  empty: document.querySelector("#message-empty"),
  transcriptPanel: document.querySelector("#transcript-panel"),
  handoffPanel: document.querySelector("#handoff-panel"),
  handoffText: document.querySelector("#handoff-text"),
  toast: document.querySelector("#toast"),
};

for (const button of document.querySelectorAll("[data-filter]")) {
  button.addEventListener("click", () => {
    state.filter = button.dataset.filter;
    for (const item of document.querySelectorAll("[data-filter]")) {
      item.classList.toggle("is-active", item === button);
    }
    renderRecords();
  });
}

for (const button of document.querySelectorAll("[data-tab]")) {
  button.addEventListener("click", () => {
    const tab = button.dataset.tab;
    for (const item of document.querySelectorAll("[data-tab]")) {
      item.classList.toggle("is-active", item === button);
      item.setAttribute("aria-selected", item === button ? "true" : "false");
    }
    elements.transcriptPanel.hidden = tab !== "transcript";
    elements.handoffPanel.hidden = tab !== "handoff";
  });
}

elements.search.addEventListener("input", () => {
  state.query = elements.search.value.trim().toLocaleLowerCase("zh-CN");
  renderRecords();
});

document.querySelector("#download-handoff").addEventListener("click", () => {
  downloadBlob(new Blob([state.markdown], { type: "text/markdown;charset=utf-8" }), "HANDOFF.md");
});
document.querySelector("#copy-handoff").addEventListener("click", async () => {
  await copyText(state.markdown);
  showToast("交接说明已复制");
});
document.querySelector("#download-package").addEventListener("click", () => {
  const project = safeFilename(readString(state.handoff?.project?.display_name) || "handoff");
  downloadBlob(new Blob([state.ciphertext], { type: "application/octet-stream" }), `Relay-${project}.relaypack`);
});
document.querySelector("#copy-link").addEventListener("click", async () => {
  await copyText(location.href);
  showToast("完整分享链接已复制");
});

void loadShare();

async function loadShare() {
  try {
    if (!globalThis.crypto?.subtle) throw new Error("browser_unsupported");
    const key = readKey();
    const pathParts = location.pathname.split("/").filter(Boolean);
    const shareId = pathParts[pathParts.length - 1] || "";
    if (!/^[A-Za-z0-9_-]{32}$/u.test(shareId)) throw new Error("share_invalid");

    const metadataResponse = await fetch(`/v1/shares/${shareId}`, {
      cache: "no-store",
      credentials: "omit",
      referrerPolicy: "no-referrer",
    });
    if (!metadataResponse.ok) throw new Error(metadataResponse.status === 404 ? "share_unavailable" : "share_download_failed");
    const metadata = await metadataResponse.json();
    if (metadata?.schema !== "relay.share.public.v1" || metadata?.status !== "ready") {
      throw new Error(metadata?.status === "awaiting_upload" ? "share_not_ready" : "share_unavailable");
    }

    const blobResponse = await fetch(`/v1/shares/${shareId}/blob`, {
      cache: "no-store",
      credentials: "omit",
      referrerPolicy: "no-referrer",
    });
    if (!blobResponse.ok) throw new Error(blobResponse.status === 404 ? "share_unavailable" : "share_download_failed");
    const ciphertext = new Uint8Array(await blobResponse.arrayBuffer());
    const expectedBytes = metadata?.ciphertext?.bytes;
    const expectedSha256 = metadata?.ciphertext?.sha256;
    if (!Number.isSafeInteger(expectedBytes) || ciphertext.byteLength !== expectedBytes) {
      throw new Error("relaypack_digest_mismatch");
    }
    const decoded = await decodeBrowserRelaypack(ciphertext, key, expectedSha256);
    state.ciphertext = ciphertext;
    state.handoff = decoded.envelope.handoff;
    state.markdown = decoded.handoffMarkdown;
    state.records = normalizeRecords(state.handoff);
    renderShare(metadata);
  } catch (error) {
    renderError(error instanceof Error ? error.message : "share_download_failed");
  }
}

function readKey() {
  const match = /^#k=([A-Za-z0-9_-]{43})$/u.exec(location.hash);
  if (!match) throw new Error("key_missing");
  return match[1];
}

function renderShare(metadata) {
  const source = isRecord(state.handoff.source) ? state.handoff.source : {};
  const project = isRecord(state.handoff.project) ? state.handoff.project : {};
  const title = readString(source.title) || "未命名会话";
  const projectName = readString(project.display_name) || "未命名项目";
  elements.title.textContent = title;
  elements.project.textContent = projectName;
  elements.agent.textContent = agentName(readString(source.agent));
  elements.expiry.textContent = formatDate(metadata.expires_at);
  elements.messageCount.textContent = String(state.records.length);
  elements.toolCount.textContent = String(state.records.reduce((count, record) =>
    count + record.blocks.filter((block) => block.isTool).length, 0));
  elements.handoffText.textContent = state.markdown;
  document.title = `${title} · Relay`;
  elements.loading.hidden = true;
  elements.viewer.hidden = false;
  renderRecords();
}

function renderRecords() {
  elements.messages.replaceChildren();
  const filtered = state.records.flatMap((record) => {
    const blocks = blocksForFilter(record.blocks);
    if (blocks.length === 0) return [];
    const searchText = `${roleLabel(record.role)} ${copyTextForBlocks(blocks)}`.toLocaleLowerCase("zh-CN");
    if (state.query && !searchText.includes(state.query)) return [];
    return [{ record, blocks }];
  });
  elements.empty.hidden = filtered.length !== 0;
  for (const item of filtered) elements.messages.append(renderRecord(item.record, item.blocks));
}

function blocksForFilter(blocks) {
  if (state.filter === "conversation") return blocks.filter((block) => !block.isTool);
  if (state.filter === "tools") return blocks.filter((block) => block.isTool);
  return blocks;
}

function renderRecord(record, visibleBlocks) {
  const article = document.createElement("article");
  article.className = `message role-${record.role}`;
  const header = document.createElement("header");
  const identity = document.createElement("div");
  identity.className = "message-identity";
  const mark = document.createElement("span");
  mark.className = "role-mark";
  mark.textContent = roleMark(record.role);
  const meta = document.createElement("span");
  const label = document.createElement("strong");
  label.textContent = roleLabel(record.role);
  const time = document.createElement("time");
  time.textContent = formatDate(record.timestamp);
  meta.append(label, time);
  identity.append(mark, meta);
  const copy = document.createElement("button");
  copy.type = "button";
  copy.textContent = "复制";
  copy.addEventListener("click", async () => {
    await copyText(copyTextForBlocks(visibleBlocks));
    showToast("这条记录已复制");
  });
  header.append(identity, copy);
  article.append(header);
  const body = document.createElement("div");
  body.className = "message-body";
  for (const block of visibleBlocks) body.append(renderBlock(block));
  article.append(body);
  return article;
}

function renderBlock(block) {
  if (block.isTool) {
    const details = document.createElement("details");
    details.className = "tool-block";
    const summary = document.createElement("summary");
    summary.textContent = block.label;
    const pre = document.createElement("pre");
    pre.textContent = block.text;
    details.append(summary, pre);
    return details;
  }
  const pre = document.createElement("pre");
  pre.className = "text-block";
  pre.textContent = block.text;
  return pre;
}

function normalizeRecords(handoff) {
  const conversation = isRecord(handoff.conversation) ? handoff.conversation : {};
  const records = Array.isArray(conversation.records) ? conversation.records : [];
  return records.flatMap((record, index) => {
    if (!isRecord(record)) return [];
    if (record.kind === "unknown") {
      const text = readString(record.safe_summary) || "此记录无法完整显示";
      const block = { text, label: `兼容性说明 · ${readString(record.original_type) || "未知记录"}`, isTool: false };
      return [{
        id: readString(record.id) || `unknown-${index}`,
        role: "unknown",
        timestamp: readString(record.timestamp),
        blocks: [block],
      }];
    }
    if (record.kind !== "message" || !Array.isArray(record.blocks)) return [];
    const blocks = record.blocks.flatMap(normalizeBlock);
    if (blocks.length === 0) return [];
    const role = readString(record.role) || "unknown";
    return [{
      id: readString(record.id) || `record-${index}`,
      role,
      timestamp: readString(record.timestamp),
      blocks,
    }];
  });
}

function copyTextForBlocks(blocks) {
  return blocks
    .map((block) => `${block.label ? `${block.label}\n` : ""}${block.text}`)
    .join("\n\n");
}

function normalizeBlock(block) {
  if (!isRecord(block)) return [];
  const kind = readString(block.kind);
  if (kind === "text") return [{ text: readString(block.text), label: "", isTool: false }];
  if (kind === "tool_call") {
    return [{
      text: formatValue(block.arguments),
      label: `工具调用 · ${readString(block.tool_name) || "未知工具"}`,
      isTool: true,
    }];
  }
  if (kind === "tool_result") {
    const content = Array.isArray(block.content) ? block.content.flatMap(normalizeBlock) : [];
    return [{
      text: content.map((item) => item.text).join("\n\n") || "没有可展示的结果",
      label: "工具结果",
      isTool: true,
    }];
  }
  if (kind === "source_context") {
    const path = readString(block.logical_path);
    return [{
      text: readString(block.text) || path || "项目说明",
      label: path ? `项目说明 · ${path}` : "项目说明",
      isTool: false,
    }];
  }
  if (kind === "asset_ref") {
    return [{ text: readString(block.caption) || "包内文件", label: "文件", isTool: false }];
  }
  if (kind === "unsupported") {
    return [{ text: readString(block.safe_summary) || "此记录无法完整显示", label: "兼容性说明", isTool: false }];
  }
  return [];
}

function renderError(code) {
  const copies = {
    key_missing: ["链接不完整", "当前地址缺少解密密钥。请让发送者重新发送完整的 Relay 分享链接。"],
    relaypack_key_invalid: ["链接不完整", "分享链接中的解密密钥格式不正确。请向发送者索取新的完整链接。"],
    relaypack_auth_failed: ["无法解密分享内容", "密钥不正确，或者分享包已经发生变化。请向发送者确认链接。"],
    share_unavailable: ["分享已经失效", "这个分享可能已经过期、被撤销，或者不存在。"],
    share_not_ready: ["分享仍在上传", "发送者的密文还没有上传完成，请稍后刷新。"],
    browser_unsupported: ["浏览器版本过旧", "当前浏览器不支持本地加密处理。请更新浏览器，或使用 Relay 桌面应用。"],
    relaypack_too_large: ["分享包过大", "浏览器无法安全读取这个分享包，请使用 Relay 桌面应用。"],
    relaypack_digest_mismatch: ["分享包校验失败", "下载内容与服务器记录不一致。请停止使用此链接并联系发送者。"],
  };
  const [title, message] = copies[code] || ["无法读取分享", "分享内容没有成功下载或验证，请稍后重试。"];
  elements.errorTitle.textContent = title;
  elements.errorMessage.textContent = message;
  elements.loading.hidden = true;
  elements.error.hidden = false;
}

function roleLabel(role) {
  if (role === "user") return "用户";
  if (role === "assistant") return "助手";
  if (role === "tool") return "工具记录";
  if (role === "developer") return "项目说明";
  if (role === "system") return "系统记录";
  return "其他记录";
}

function roleMark(role) {
  if (role === "user") return "用";
  if (role === "assistant") return "助";
  if (role === "tool") return "工";
  return "记";
}

function agentName(agent) {
  return agent === "claude_code" ? "Claude Code" : "ChatGPT";
}

function formatDate(value) {
  if (!value || Number.isNaN(Date.parse(value))) return "时间未知";
  return new Intl.DateTimeFormat("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(value));
}

function formatValue(value) {
  if (typeof value === "string") return value;
  try { return JSON.stringify(value, null, 2); } catch { return String(value ?? ""); }
}

function readString(value) {
  return typeof value === "string" ? value : "";
}

function isRecord(value) {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function safeFilename(value) {
  return value.normalize("NFKC").replace(/[^\p{L}\p{N}._-]+/gu, "-").replace(/-+/g, "-").replace(/^-|-$/g, "").slice(0, 48) || "handoff";
}

function downloadBlob(blob, filename) {
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement("a");
  anchor.href = url;
  anchor.download = filename;
  document.body.append(anchor);
  anchor.click();
  anchor.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1_000);
}

async function copyText(value) {
  if (navigator.clipboard?.writeText) {
    await navigator.clipboard.writeText(value);
    return;
  }
  const area = document.createElement("textarea");
  area.value = value;
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.append(area);
  area.select();
  document.execCommand("copy");
  area.remove();
}

let toastTimer;
function showToast(message) {
  elements.toast.textContent = message;
  elements.toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { elements.toast.hidden = true; }, 2_400);
}
