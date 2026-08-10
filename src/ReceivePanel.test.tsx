import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";
import ReceivePanel, {
  importReceivedSession,
  nativeImportActionState,
  nativeContinueCommand,
  nativeImportOpenNotice,
  nativeImportMessage,
  receiveSaveCopy,
  receiveTargetCopy,
  receiveErrorMessage,
} from "./ReceivePanel";
import type {
  ImportNativeSessionResult,
  RestoreRelaypackResult,
} from "./types";

const restored: RestoreRelaypackResult = {
  worktree_path: "/tmp/relay/project",
  branch_name: null,
  head: null,
  handoff_directory: "/tmp/relay/project/.relay",
  handoff_markdown_path: "/tmp/relay/project/HANDOFF.md",
  handoff_json_path: "/tmp/relay/project/handoff.json",
  staged_applied: false,
  unstaged_applied: false,
  untracked_files_restored: 0,
  preview: {} as RestoreRelaypackResult["preview"],
};

function importResult(
  overrides: Partial<ImportNativeSessionResult> = {},
): ImportNativeSessionResult {
  return {
    status: "ok",
    target: "codex",
    session_id: "019c1234-5678-7000-8000-123456789abc",
    title: "Relay 验收 · 2026-08-10 · 019c1234",
    target_home: "/tmp/codex-home",
    target_cwd: restored.worktree_path,
    session_path: "/tmp/codex-home/sessions/task.jsonl",
    writes: [],
    created_files: ["/tmp/codex-home/sessions/task.jsonl"],
    dry_run: false,
    verification: { session_file: true, index: true, state: true, pinned: true },
    open_status: "opened",
    ...overrides,
  };
}

describe("接收分享包", () => {
  it("第一次导入会先保存内容，再创建所选会话", async () => {
    const restore = vi.fn(async () => restored);
    const importer = vi.fn(async () => importResult());

    const attempt = await importReceivedSession("codex", null, restore, importer);

    expect(restore).toHaveBeenCalledTimes(1);
    expect(importer).toHaveBeenCalledWith({
      agent: "codex",
      worktree_path: restored.worktree_path,
      handoff_json_path: restored.handoff_json_path,
    });
    expect(attempt.result?.session_id).toContain("019c1234");
  });

  it("会话导入失败后重试不会再次创建接收目录", async () => {
    const restore = vi.fn(async () => restored);
    const importer = vi
      .fn()
      .mockRejectedValueOnce({ code: "native_import_failed", message: "写入失败" })
      .mockResolvedValueOnce(importResult());

    const first = await importReceivedSession("codex", null, restore, importer);
    expect(first.restored).toBe(restored);
    expect(first.result).toBeNull();

    const second = await importReceivedSession("codex", first.restored, restore, importer);
    expect(second.result?.status).toBe("ok");
    expect(restore).toHaveBeenCalledTimes(1);
    expect(importer).toHaveBeenCalledTimes(2);
  });

  it("ChatGPT 未能显示时仍明确说明任务已经导入", () => {
    const message = nativeImportMessage(importResult({
      open_status: "failed",
      open_error: "没有找到经过签名检查的 ChatGPT 应用",
    }));
    expect(message).toContain("已经导入");
    expect(message).toContain("未能显示 ChatGPT");
  });

  it("ChatGPT 导入成功后说明任务已经直接打开", () => {
    const message = nativeImportMessage(importResult({ open_status: "opened" }));
    expect(message).toContain("已经导入并打开");
    expect(message).not.toContain("任务列表顶部");
    expect(nativeImportOpenNotice(importResult({ open_status: "opened" }))).toBeNull();
  });

  it("ChatGPT 运行中时说明已经刷新任务列表", () => {
    const result = importResult({
      open_status: "opened",
      catalog_refresh_status: "sent",
    });
    expect(nativeImportMessage(result)).toContain("已经导入并打开");
    expect(nativeImportOpenNotice(result)).toBeNull();
  });

  it("ChatGPT 任务列表刷新失败时保留导入结果并给出恢复步骤", () => {
    const result = importResult({
      open_status: "opened",
      catalog_refresh_status: "failed",
      catalog_refresh_error_code: "chatgpt_catalog_refresh_failed",
      catalog_refresh_error: "connection reset",
    });
    expect(nativeImportMessage(result)).toContain("已经导入");
    expect(nativeImportOpenNotice(result)).toBeNull();
  });

  it("ChatGPT 需要手动打开时不显示内部英文错误", () => {
    const result = importResult({
      open_status: "manual",
      open_error_code: "chatgpt_handler_not_found",
      open_error: "no ChatGPT application is registered to open codex:// links",
    });
    expect(nativeImportMessage(result)).toContain("标题或编号");
    expect(nativeImportOpenNotice(result)).toBe(
      "任务已经导入。Relay 未找到可显示的 ChatGPT 应用，请打开 ChatGPT 后按标题或任务编号查找。",
    );
    expect(nativeImportOpenNotice(result)).not.toContain("codex://");
  });

  it("ChatGPT 签名检查失败时给出正式中文说明", () => {
    expect(nativeImportOpenNotice(importResult({
      open_status: "failed",
      open_error_code: "chatgpt_identity_unverified",
      open_error: "no registered ChatGPT application passed the OpenAI signature check",
    }))).toContain("官方 ChatGPT 应用");
  });

  it("Claude Code 显示可复制的继续命令", () => {
    expect(nativeContinueCommand(importResult({
      target: "claude_code",
      open_status: "manual",
      continue_command: "claude --resume claude-session-id",
    }))).toBe("claude --resume claude-session-id");

    expect(nativeContinueCommand(importResult({
      target: "claude_code",
      open_status: "manual",
    }))).toBe("");
  });

  it("没有可导入内容时使用普通中文说明", () => {
    expect(receiveErrorMessage({
      code: "no_importable_content",
      message: "the share contains no visible conversation",
    })).toBe("发送者未包含可导入的聊天记录或项目说明。已接收的文件仍可保留。");
  });

  it("ChatGPT 尚未初始化时说明下一步操作", () => {
    expect(receiveErrorMessage({
      code: "chatgpt_state_not_found",
      message: "ChatGPT task database was not found",
    })).toBe("没有找到 ChatGPT 的本机任务数据。请先打开一次 ChatGPT，再返回 Relay 重新导入。");
  });

  it("导入器写入失败时不直接显示英文内部说明", () => {
    expect(receiveErrorMessage({
      code: "index_write_failed",
      message: "cannot update the ChatGPT task index",
    })).toBe("会话文件已经撤销，但会话列表更新失败。");
  });

  it("所选目录不是 Git 仓库时说明应该选择什么", () => {
    expect(receiveErrorMessage({
      code: "not_a_git_repository",
      message: "'/Users/demo/Downloads' is not inside a Git worktree",
    })).toBe("所选文件夹不是 Git 仓库。请选择这个项目在本机的 Git 仓库根目录。");
  });

  it("所选仓库不是同一个项目时给出明确说明", () => {
    expect(receiveErrorMessage({
      code: "repository_identity_mismatch",
      message: "receiver repository does not have a matching fetch remote",
    })).toBe("所选 Git 仓库与发送者的项目不一致。请重新选择同一个项目的本机仓库。");
  });

  it("ChatGPT 无法显示时使用普通中文说明", () => {
    expect(receiveErrorMessage({
      code: "chatgpt_open_failed",
      message: "macOS did not confirm the ChatGPT launch request in time",
    })).toBe("macOS 未能显示 ChatGPT。请手动打开官方 ChatGPT 应用。");
  });

  it("自动撤销不完整时提示保留备份", () => {
    expect(receiveErrorMessage({
      code: "rollback_incomplete",
      message: "automatic rollback also failed",
    })).toContain("保留导入前备份");
  });

  it("页面直接提供两个导入目标和只保存文件入口", () => {
    const markup = renderToStaticMarkup(
      <ReceivePanel home="/Users/demo" onNotice={() => undefined} />,
    );
    expect(receiveTargetCopy.codex.title).toBe("导入到 ChatGPT");
    expect(receiveTargetCopy.claude_code.title).toBe("导入到 Claude Code");
    expect(receiveSaveCopy.title).toBe("只保存文件");
    expect(markup).toContain("打开后可查看内容并继续工作");
    expect(markup).toContain("打开分享");
    expect(markup).not.toContain("分享内容已验证");
    expect(markup).not.toContain("原生历史");
    expect(markup).not.toContain("交接文件夹");
    expect(markup).not.toContain("输入 .relaypack 文件路径");
  });

  it("成功导入一个目标后只禁用该目标", () => {
    const chatgpt = nativeImportActionState("codex", true, null, ["codex"], null);
    const claude = nativeImportActionState("claude_code", true, null, ["codex"], null);

    expect(chatgpt).toEqual({
      label: "已导入到 ChatGPT",
      disabled: true,
      complete: true,
    });
    expect(claude).toEqual({
      label: "导入到 Claude Code",
      disabled: false,
      complete: false,
    });
  });
});
