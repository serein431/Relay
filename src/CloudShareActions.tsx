import { useState } from "react";
import { revokeSavedShare, uploadShare } from "./lib/tauri";
import type { ExportRelaypackResult, UploadShareResult } from "./types";

type CloudShareActionsProps = {
  pack: ExportRelaypackResult;
  onNotice: (message: string) => void;
  onHistoryChanged?: () => void;
};

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  if (typeof error === "object" && error !== null && "message" in error) {
    return String((error as { message: unknown }).message);
  }
  return String(error);
}

function isPendingUploadError(error: unknown): boolean {
  if (typeof error !== "object" || error === null) return false;
  const value = error as { details?: unknown };
  if (typeof value.details !== "object" || value.details === null) return false;
  const details = value.details as { upload_pending?: unknown; can_resume?: unknown };
  return details.upload_pending === true && details.can_resume === true;
}

export default function CloudShareActions({
  pack,
  onNotice,
  onHistoryChanged,
}: CloudShareActionsProps) {
  const [serviceBaseUrl, setServiceBaseUrl] = useState(
    () => window.localStorage.getItem("relay.shareService") ?? "http://127.0.0.1:8787",
  );
  const [expiresInSeconds, setExpiresInSeconds] = useState(7 * 24 * 60 * 60);
  const [uploadToken, setUploadToken] = useState("");
  const [share, setShare] = useState<UploadShareResult | null>(null);
  const [revoked, setRevoked] = useState(false);
  const [revokeConfirming, setRevokeConfirming] = useState(false);
  const [busy, setBusy] = useState<"upload" | "revoke" | null>(null);
  const [error, setError] = useState<string | null>(null);

  const upload = async () => {
    setBusy("upload");
    setError(null);
    try {
      const base = serviceBaseUrl.trim();
      window.localStorage.setItem("relay.shareService", base);
      const result = await uploadShare({
        package_path: pack.package_path,
        key: pack.key_fragment,
        service_base_url: base,
        project_title: pack.preview.title,
        project_name: pack.preview.project_name,
        expires_in_seconds: expiresInSeconds,
        upload_token: uploadToken.trim() || undefined,
      });
      setShare(result);
      setRevoked(false);
      onHistoryChanged?.();
      await navigator.clipboard.writeText(result.share_url);
      onNotice("加密分享链接已生成并复制，撤销凭据已经保存在本机。");
    } catch (caught) {
      if (isPendingUploadError(caught)) {
        onHistoryChanged?.();
        setError(
          "分享凭据已经安全保存在本机，但密文上传尚未确认完成。请到“分享记录”继续上传或撤销。",
        );
        onNotice("上传尚未完成，可以从分享记录继续或撤销。");
      } else {
        setError(errorMessage(caught));
      }
    } finally {
      setBusy(null);
    }
  };

  const copy = async () => {
    if (!share) return;
    await navigator.clipboard.writeText(share.share_url);
    onNotice("分享链接已复制。");
  };

  const revoke = async () => {
    if (!share) return;
    if (!revokeConfirming) {
      setRevokeConfirming(true);
      window.setTimeout(() => setRevokeConfirming(false), 5000);
      return;
    }
    setBusy("revoke");
    setError(null);
    try {
      await revokeSavedShare({ share_id: share.share_id });
      setRevoked(true);
      setRevokeConfirming(false);
      onHistoryChanged?.();
      onNotice("这个分享已经撤销，旧链接不能再下载密文包。");
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setBusy(null);
    }
  };

  return (
    <section className="cloud-share-actions">
      <div className="cloud-share-heading">
        <div><span className="eyebrow">发送方式一 · 推荐</span><h4>生成分享链接</h4></div>
        <span>只上传密文</span>
      </div>

      {share ? (
        <>
          <label className="share-url-field">
            <span>{revoked ? "已撤销链接" : "完整分享链接"}</span>
            <code className={revoked ? "is-revoked" : ""}>{share.share_url}</code>
          </label>
          <div className="share-expiry">
            <span>到期时间</span><strong>{new Date(share.expires_at).toLocaleString("zh-CN")}</strong>
          </div>
          <p className="share-capability-note">接收者无需安装 Relay，即可直接在浏览器中查看聊天记录和交接说明；恢复代码或创建本机任务时才需要桌面应用。</p>
          <p className="share-capability-note">链接中 # 后面的密钥不会随 HTTP 请求发送，但聊天软件、剪贴板历史和截图仍可能看到完整链接。</p>
          <p className="share-capability-note">撤销所需的令牌只保存在 Relay 的本机数据目录，不会显示在页面里。</p>
          <div className="cloud-share-buttons">
            <button type="button" onClick={() => void revoke()} disabled={busy !== null || revoked}>
              {busy === "revoke" ? "正在撤销" : revoked ? "已经撤销" : revokeConfirming ? "再点一次确认撤销" : "撤销链接"}
            </button>
            <button type="button" className="is-primary" onClick={() => void copy()} disabled={revoked}>
              复制链接
            </button>
          </div>
        </>
      ) : (
        <>
          <p className="cloud-share-intro">接收者可以直接在浏览器中查看分享，无需先安装 Relay。拥有完整链接的人都能读取内容；上传前请确认接收者和有效期。</p>
          <div className="cloud-share-grid">
            <label>
              <span>分享服务</span>
              <input value={serviceBaseUrl} onChange={(event) => setServiceBaseUrl(event.target.value)} spellCheck={false} />
            </label>
            <label>
              <span>有效期</span>
              <select value={expiresInSeconds} onChange={(event) => setExpiresInSeconds(Number(event.target.value))}>
                <option value={24 * 60 * 60}>1 天</option>
                <option value={7 * 24 * 60 * 60}>7 天</option>
                <option value={30 * 24 * 60 * 60}>30 天</option>
              </select>
            </label>
          </div>
          <label className="upload-token-field">
            <span>服务上传令牌（可选）</span>
            <input type="password" value={uploadToken} onChange={(event) => setUploadToken(event.target.value)} autoComplete="off" />
          </label>
          {error ? <p className="cloud-share-error">{error}</p> : null}
          <button className="cloud-upload-button" type="button" onClick={() => void upload()} disabled={busy !== null || !serviceBaseUrl.trim()}>
            {busy === "upload" ? "正在上传密文" : "生成分享链接并复制"}
          </button>
        </>
      )}
      {share && error ? <p className="cloud-share-error">{error}</p> : null}
    </section>
  );
}
