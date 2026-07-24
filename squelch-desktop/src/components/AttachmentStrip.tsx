// Attachment strip for the thread viewer — one flat card per attachment, shown
// under a message body (above the frame's links row). Card variants:
//   * image/* (not svg): a real thumbnail, fetched LAZILY on the card's mount
//     over the authenticated door (bearer auth can't ride an <img src>, so bytes
//     flow fetch -> Blob -> objectURL, revoked on unmount). Skeleton while loading.
//   * application/pdf: a document glyph card; clicking opens the PDF preview
//     overlay (WKWebView renders PDFs natively via <embed>).
//   * everything else, AND image/svg+xml (scriptable — never rendered inline):
//     a file glyph card with filename + human size.
// downloadable=false (bytes over the ingest cap, metadata only) => dimmed card,
// "too large — not stored", no thumbnail/preview/download.
//
// SECURITY: nothing here trusts the attachment mime for rendering beyond the
// image/pdf/other bucket choice; the SERVER decides what Content-Type it will
// actually serve (svg + html/xml always come back as octet-stream), so a
// mislabeled attachment can't become a scriptable object from our origin.

import { useEffect, useMemo, useRef, useState } from "react";
import { Download, File as FileIcon, FileText } from "lucide-react";
import { api, ApiError } from "../api";
import type { Attachment } from "../api";
import { useStore } from "../state";
import { useKeys, useKeyContext } from "../keys";
import { humanSize } from "../lib/format";
import { saveBytes } from "../lib/download";

/** Images above this skip the auto-fetched thumbnail (a 10MB photo for a
 *  120px thumb is silly bandwidth) and show the glyph card instead. */
const THUMB_MAX_BYTES = 2 * 1024 * 1024;

/** True for a raster image we can safely render as a thumbnail (svg excluded —
 *  it is scriptable and the server refuses to serve it as image/*). */
function isThumbnailable(mime: string, size: number): boolean {
  return (
    mime.startsWith("image/") &&
    mime !== "image/svg+xml" &&
    size <= THUMB_MAX_BYTES
  );
}

function isPdf(mime: string): boolean {
  return mime === "application/pdf";
}

export function AttachmentStrip({
  attachments,
}: {
  attachments: Attachment[];
}) {
  const pushToast = useStore((s) => s.pushToast);
  // The attachment whose PDF preview overlay is open (null = closed).
  const [preview, setPreview] = useState<Attachment | null>(null);

  const download = async (att: Attachment) => {
    try {
      const { bytes, filename } = await api.fetchAttachment(
        att.id,
        att.filename,
      );
      const result = await saveBytes(bytes, filename);
      if (result === "saved") pushToast(`saved ${filename}`, "success");
    } catch (e) {
      pushToast(
        e instanceof ApiError ? e.message : "download failed",
        "error",
      );
    }
  };

  if (attachments.length === 0) return null;

  return (
    <>
      <div className="att-strip" aria-label="attachments">
        {attachments.map((att) => (
          <AttachmentCard
            key={att.id}
            att={att}
            onDownload={() => void download(att)}
            onPreview={
              isPdf(att.mime) ? () => setPreview(att) : undefined
            }
          />
        ))}
      </div>
      {preview && (
        <PdfPreview
          att={preview}
          onDownload={() => void download(preview)}
          onClose={() => setPreview(null)}
        />
      )}
    </>
  );
}

function AttachmentCard({
  att,
  onDownload,
  onPreview,
}: {
  att: Attachment;
  onDownload: () => void;
  onPreview?: () => void;
}) {
  const stored = att.downloadable;
  const thumb = stored && isThumbnailable(att.mime, att.size);
  const pdf = stored && isPdf(att.mime);

  // A pdf card is a button (opens preview); everything else is a static card
  // with an explicit download affordance. The click target for the pdf is the
  // whole card body, but the download button always stops propagation.
  const clickable = pdf && onPreview;

  return (
    <div
      className={`att-card${stored ? "" : " att-unstored"}${clickable ? " att-clickable" : ""}`}
      onClick={clickable ? onPreview : undefined}
      title={att.filename}
    >
      <div className="att-thumb">
        {thumb ? (
          <ThumbImage att={att} />
        ) : pdf ? (
          <FileText size={22} className="att-glyph" aria-hidden />
        ) : (
          <FileIcon size={22} className="att-glyph" aria-hidden />
        )}
      </div>
      <div className="att-meta">
        <span className="att-name">{att.filename}</span>
        <span className="att-sub">
          {stored ? humanSize(att.size) : "too large — not stored"}
        </span>
      </div>
      {stored && (
        <button
          type="button"
          className="att-dl"
          title={`download ${att.filename}`}
          onClick={(e) => {
            e.stopPropagation();
            onDownload();
          }}
        >
          <Download size={14} aria-hidden />
        </button>
      )}
    </div>
  );
}

/**
 * A lazily-fetched image thumbnail. Fetches the bytes on mount, holds the
 * objectURL in state, and revokes it on unmount. A skeleton box shows while
 * loading; a fetch failure falls back to the generic file glyph.
 */
function ThumbImage({ att }: { att: Attachment }) {
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    let alive = true;
    let objectUrl: string | null = null;
    api
      .fetchAttachment(att.id, att.filename)
      .then(({ bytes, mime }) => {
        if (!alive) return;
        objectUrl = URL.createObjectURL(new Blob([bytes], { type: mime }));
        setUrl(objectUrl);
      })
      .catch(() => {
        if (alive) setFailed(true);
      });
    return () => {
      alive = false;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [att.id, att.filename]);

  if (failed) return <FileIcon size={22} className="att-glyph" aria-hidden />;
  if (!url) return <div className="att-skeleton" aria-hidden />;
  return <img className="att-img" src={url} alt={att.filename} />;
}

/**
 * Fullscreen-ish PDF preview overlay. Own KeyContext ("modal") so Esc closes it
 * without leaking to the thread underneath. Fetches the pdf bytes on mount into
 * a blob objectURL (revoked on unmount) and renders it in a native <embed>
 * (WKWebView renders application/pdf inline). CSP must permit object-src /
 * frame-src blob: — see index.html + tauri.conf.json (kept in lockstep).
 */
function PdfPreview({
  att,
  onDownload,
  onClose,
}: {
  att: Attachment;
  onDownload: () => void;
  onClose: () => void;
}) {
  const [url, setUrl] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // Keep the fetched bytes so the overlay's download button reuses them
  // instead of a second full-body round trip.
  const bytesRef = useRef<{ bytes: ArrayBuffer; filename: string } | null>(null);

  useKeyContext("modal");
  const bindings = useMemo(
    () => [
      {
        key: "Escape",
        description: "close preview",
        allowInInput: true,
        handler: () => onClose(),
      },
    ],
    [onClose],
  );
  useKeys("modal", bindings, [bindings]);

  useEffect(() => {
    let alive = true;
    let objectUrl: string | null = null;
    api
      .fetchAttachment(att.id, att.filename)
      .then(({ bytes, mime, filename }) => {
        if (!alive) return;
        bytesRef.current = { bytes, filename };
        objectUrl = URL.createObjectURL(
          new Blob([bytes], { type: mime || "application/pdf" }),
        );
        setUrl(objectUrl);
      })
      .catch((e) => {
        if (alive)
          setError(e instanceof ApiError ? e.message : "preview failed");
      });
    return () => {
      alive = false;
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [att.id, att.filename]);

  return (
    <div className="pdf-overlay" onClick={onClose}>
      <div className="pdf-frame" onClick={(e) => e.stopPropagation()}>
        <header className="pdf-head" data-tauri-drag-region>
          <span className="pdf-title">{att.filename}</span>
          <button
            type="button"
            className="pdf-dl"
            onClick={() => {
              // Reuse the bytes already fetched for the preview; only fall
              // back to the shared (re-fetching) path if they aren't held.
              const held = bytesRef.current;
              if (held) void saveBytes(held.bytes, held.filename);
              else onDownload();
            }}
            title={`download ${att.filename}`}
          >
            <Download size={14} aria-hidden /> download
          </button>
          <span className="pdf-esc">
            <kbd>Esc</kbd> close
          </span>
        </header>
        <div className="pdf-body">
          {error ? (
            <div className="side-error">{error}</div>
          ) : !url ? (
            <div className="side-loading">loading pdf…</div>
          ) : (
            <embed
              className="pdf-embed"
              src={url}
              type="application/pdf"
            />
          )}
        </div>
      </div>
    </div>
  );
}
