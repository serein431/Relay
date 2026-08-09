import type { SessionSummary } from "../types";

const DISPLAY_TITLE_LIMIT = 34;
const UUID_TITLE = /^(?:[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}|[0-9a-f]{32})$/i;
const UNTITLED_TITLES = new Set(["未命名会话", "untitled session", "untitled"]);

export function sessionKey(session: SessionSummary): string {
  return `${session.agent}:${session.id}:${session.sourcePath ?? ""}`;
}

function normalizeText(value: string): string {
  return value.replace(/\s+/g, " ").trim();
}

function truncateText(value: string, limit = DISPLAY_TITLE_LIMIT): string {
  const characters = Array.from(value);
  if (characters.length <= limit) return value;
  return `${characters.slice(0, limit - 3).join("")}...`;
}

function previewTitle(preview?: string): string {
  if (!preview) return "";
  const cleaned = normalizeText(preview)
    .replace(/^[\s✅☑✔️🔧🟢🟡🔴•·\-—:：#>*_`]+/u, "")
    .replace(/[`*_]/g, "");
  if (!cleaned) return "";

  const characters = Array.from(cleaned);
  const hardPunctuation = new Set(["。", "！", "？", "!", "?", "；", ";"]);
  const softPunctuation = new Set(["，", ",", "：", ":"]);
  const hardEnd = characters.findIndex(
    (character, index) => index >= 7 && hardPunctuation.has(character),
  );
  const softEnd = characters.findIndex(
    (character, index) => index >= 9 && softPunctuation.has(character),
  );
  const sentence = softEnd >= 0 && (hardEnd < 0 || softEnd < hardEnd)
    ? characters.slice(0, softEnd).join("")
    : hardEnd >= 0
      ? characters.slice(0, hardEnd + 1).join("")
      : cleaned;
  return truncateText(sentence);
}

function shortSessionId(session: SessionSummary): string {
  const compact = session.id.replace(/[^a-zA-Z0-9]/g, "");
  return compact.slice(0, 8) || "session";
}

function hasUsefulTitle(session: SessionSummary): boolean {
  const title = normalizeText(session.title);
  if (!title) return false;
  if (title === normalizeText(session.id)) return false;
  if (UNTITLED_TITLES.has(title.toLocaleLowerCase("zh-CN"))) return false;
  return !UUID_TITLE.test(title);
}

export function sessionDateTitle(value?: string): string {
  if (!value) return "";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "";
  const month = date.getMonth() + 1;
  const day = date.getDate();
  const hour = String(date.getHours()).padStart(2, "0");
  const minute = String(date.getMinutes()).padStart(2, "0");
  return `${month}月${day}日 ${hour}:${minute}`;
}

function baseDisplayTitle(session: SessionSummary): { title: string; generated: boolean } {
  if (hasUsefulTitle(session)) {
    return { title: normalizeText(session.title), generated: false };
  }
  const dated = sessionDateTitle(session.createdAt ?? session.updatedAt);
  if (dated) return { title: dated, generated: true };
  return { title: `临时会话 · ${shortSessionId(session)}`, generated: true };
}

export function uniqueSessionDisplayTitles(
  sessions: SessionSummary[],
): Map<string, string> {
  const groups = new Map<string, SessionSummary[]>();
  for (const session of sessions) {
    const title = baseDisplayTitle(session).title;
    const key = title.toLocaleLowerCase("zh-CN");
    const group = groups.get(key) ?? [];
    group.push(session);
    groups.set(key, group);
  }

  const result = new Map<string, string>();
  const usedTitles = new Set<string>();
  for (const session of sessions) {
    const base = baseDisplayTitle(session);
    const baseTitle = base.title;
    const duplicates = groups.get(baseTitle.toLocaleLowerCase("zh-CN")) ?? [];
    let displayTitle = duplicates.length > 1
      ? base.generated
        ? `${truncateText(baseTitle)} · ${shortSessionId(session)}`
        : previewTitle(session.preview) || `${truncateText(baseTitle)} · ${shortSessionId(session)}`
      : baseTitle;

    const normalizedDisplayTitle = displayTitle.toLocaleLowerCase("zh-CN");
    if (usedTitles.has(normalizedDisplayTitle)) {
      displayTitle = `${truncateText(displayTitle, 26)} · ${shortSessionId(session)}`;
    }
    usedTitles.add(displayTitle.toLocaleLowerCase("zh-CN"));
    result.set(sessionKey(session), displayTitle);
  }
  return result;
}
