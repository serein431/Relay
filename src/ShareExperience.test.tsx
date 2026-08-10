import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import CloudShareActions from "./CloudShareActions";
import ReceivePanel from "./ReceivePanel";
import {
  DEFAULT_SHARE_SERVICE_BASE_URL,
  shareServiceOriginFromLink,
} from "./lib/share-service";
import type { ExportRelaypackResult } from "./types";

describe("分享链接界面", () => {
  it("发送页不再要求填写服务地址或上传令牌", () => {
    const pack = {
      package_path: "/tmp/example.relaypack",
      key_fragment: "A".repeat(43),
      preview: { title: "示例会话", project_name: "Relay" },
    } as ExportRelaypackResult;
    const markup = renderToStaticMarkup(
      <CloudShareActions pack={pack} onNotice={() => undefined} />,
    );
    expect(markup).toContain("有效期");
    expect(markup).toContain("链接有效期内，任何持有者都可以查看分享内容");
    expect(markup).not.toContain("服务上传令牌");
    expect(markup).not.toContain("分享服务</span><input");
  });

  it("自动上传时不重复要求选择有效期，也不显示内部上传说明", () => {
    const pack = {
      package_path: "/tmp/example.relaypack",
      key_fragment: "A".repeat(43),
      preview: { title: "示例会话", project_name: "Relay" },
    } as ExportRelaypackResult;
    const markup = renderToStaticMarkup(
      <CloudShareActions pack={pack} autoUpload onNotice={() => undefined} />,
    );
    expect(markup).toContain("正在准备分享链接");
    expect(markup).not.toContain("<span>有效期</span>");
    expect(markup).not.toContain("撤销凭据");
    expect(markup).not.toContain("密文");
  });

  it("接收页从完整链接读取服务来源", () => {
    const markup = renderToStaticMarkup(
      <ReceivePanel home="/Users/demo" onNotice={() => undefined} />,
    );
    expect(markup).toContain("分享链接");
    expect(markup).toContain("粘贴 Relay 分享链接");
    expect(markup).not.toContain("可信分享服务");
    expect(markup).not.toContain("workers.dev");
    expect(
      shareServiceOriginFromLink(`${DEFAULT_SHARE_SERVICE_BASE_URL}/s/v1/${"S".repeat(32)}#k=${"K".repeat(43)}`),
    ).toBe(DEFAULT_SHARE_SERVICE_BASE_URL);
  });

  it("拒绝普通 HTTP 分享地址", () => {
    expect(() => shareServiceOriginFromLink("http://example.com/s/v1/test#k=test"))
      .toThrow("必须使用 HTTPS");
    expect(shareServiceOriginFromLink("http://127.0.0.1:8787/s/v1/test#k=test"))
      .toBe("http://127.0.0.1:8787");
  });
});
