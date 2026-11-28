import { Button, Spinner, Text } from "@fluentui/react-components";
import { ImageOffRegular, OpenRegular, PlayCircleFilled } from "@fluentui/react-icons";
import { useEffect, useRef, useState } from "react";

import { api } from "../lib/api";
import { previewKind } from "../lib/format";
import { useI18n } from "../lib/i18n";
import { FileIcon } from "./FileVisuals";
import { Markdown } from "./Markdown";

interface PreviewProps {
  /** Where to load the file from: the public URL, or a presigned one. */
  url: string | null;
  filename: string;
  mimeType?: string;
  /** Opened by "Open in Browser" (the public link, even when `url` is presigned). */
  browserURL: string;
  /** A local thumbnail to show, dimmed, while the full image loads, and in
   * place of a preview for other files (a video's frame, a document's
   * first page). */
  placeholder?: string | null;
  /** The link a video or audio file plays from, asked for on Play: a
   * presigned one where possible, so private buckets work too. Without it,
   * `url`. */
  mediaURL?: () => Promise<string | null>;
  /** The file's size in bytes, when known: past 25 MB, nothing is downloaded. */
  size?: number;
  onZoom?: () => void;
  /** Reads the file through the destination's own storage, for a WebDAV
   * or SFTP folder whose public link doesn't load (no web server, or one
   * that wants a password): tried once the link fails, or straight away
   * without a link (docs/storage-backends.md, Phase 4). */
  throughStorage?: () => Promise<ArrayBuffer>;
}

/** A type for a file read through storage, so the web view shows it. */
function blobType(filename: string, mimeType?: string): string {
  if (mimeType) return mimeType;
  const extension = filename.split(".").pop()?.toLowerCase() ?? "";
  const types: Record<string, string> = {
    png: "image/png",
    jpg: "image/jpeg",
    jpeg: "image/jpeg",
    gif: "image/gif",
    webp: "image/webp",
    avif: "image/avif",
    bmp: "image/bmp",
    ico: "image/x-icon",
    svg: "image/svg+xml",
    mp4: "video/mp4",
    m4v: "video/mp4",
    webm: "video/webm",
    mov: "video/quicktime",
    mp3: "audio/mpeg",
    m4a: "audio/mp4",
    wav: "audio/wav",
    ogg: "audio/ogg",
    flac: "audio/flac",
    pdf: "application/pdf",
  };
  return types[extension] ?? "application/octet-stream";
}

/** An object URL of the file read through storage, made once `wanted`,
 * and given back when it's no longer shown. Null until then, or when it
 * can't be read ("failed"). */
function useStorageURL(throughStorage: (() => Promise<ArrayBuffer>) | undefined, wanted: boolean, type: string, resetKey: string | null) {
  const [state, setState] = useState<{ url: string | null; failed: boolean }>({ url: null, failed: false });
  const storage = useRef(throughStorage);
  storage.current = throughStorage;
  const available = throughStorage !== undefined;
  useEffect(() => setState({ url: null, failed: false }), [resetKey]);
  useEffect(() => {
    const read = storage.current;
    if (!wanted || !read || state.url || state.failed) return;
    let cancelled = false;
    let made: string | null = null;
    read()
      .then((data) => {
        if (cancelled) return;
        made = URL.createObjectURL(new Blob([data], { type }));
        setState({ url: made, failed: false });
      })
      .catch(() => !cancelled && setState({ url: null, failed: true }));
    return () => {
      cancelled = true;
    };
  }, [wanted, available, state.url, state.failed, type]);
  useEffect(() => {
    const url = state.url;
    return () => {
      if (url) URL.revokeObjectURL(url);
    };
  }, [state.url]);
  return { url: state.url, failed: state.failed || (wanted && !throughStorage) };
}

/** An image from its link, or read through storage when the link doesn't
 * load (see `throughStorage`). For the zoomed view. */
export function FallbackImage({ url, filename, mimeType, throughStorage }: { url: string | null; filename: string; mimeType?: string; throughStorage?: () => Promise<ArrayBuffer> }) {
  const [linkFailed, setLinkFailed] = useState(false);
  useEffect(() => setLinkFailed(false), [url]);
  const stored = useStorageURL(throughStorage, linkFailed || !url, blobType(filename, mimeType), url);
  const src = linkFailed || !url ? stored.url : url;
  return src ? <img src={src} alt="" draggable={false} onError={() => setLinkFailed(true)} /> : null;
}

type Loaded = { state: "loading" } | { state: "failed" } | { state: "ready"; data: ArrayBuffer };

/** The biggest file downloaded for a preview; Rust stops there too. */
const MAX_PREVIEW_BYTES = 25 * 1024 * 1024;

/** Downloads a file for an inline preview (PDF, text, Markdown). The
 * uploaded file isn't kept locally, so this fetches it each time; from its
 * link, else through storage when there's a way (`throughStorage`). */
function useRemoteFile(url: string | null, enabled: boolean, throughStorage?: () => Promise<ArrayBuffer>): Loaded {
  const [loaded, setLoaded] = useState<Loaded>({ state: "loading" });
  // The latest one, without fetching again when a caller makes a new one.
  const storage = useRef(throughStorage);
  storage.current = throughStorage;
  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    setLoaded({ state: "loading" });
    const throughStorage = storage.current;
    const fromStorage = () =>
      throughStorage
        ? throughStorage()
            .then((data) => !cancelled && setLoaded({ state: "ready", data }))
            .catch(() => !cancelled && setLoaded({ state: "failed" }))
        : setLoaded({ state: "failed" });
    if (!url) {
      if (throughStorage) void fromStorage();
      return;
    }
    api
      .fetchRemote(url)
      .then((data) => !cancelled && setLoaded({ state: "ready", data }))
      .catch(() => !cancelled && void fromStorage());
    return () => {
      cancelled = true;
    };
  }, [url, enabled]);
  return loaded;
}

function Unavailable({ browserURL }: { browserURL: string }) {
  const { t } = useI18n();
  return (
    <div className="preview-message">
      <ImageOffRegular fontSize={28} />
      <Text size={200}>{t("Preview unavailable")}</Text>
      <Button size="small" appearance="transparent" icon={<OpenRegular />} onClick={() => api.openUrl(browserURL)}>
        {t("Open in Browser")}
      </Button>
    </div>
  );
}

/** A video or audio file, played from its link without being downloaded
 * first. Nothing loads until Play is clicked; until then its thumbnail
 * stands in. */
function MediaPlayer(props: {
  video: boolean;
  filename: string;
  poster?: string | null;
  load: () => Promise<string | null>;
  browserURL: string;
  /** Read through storage when the link doesn't play (up to 25 MB). */
  throughStorage?: () => Promise<ArrayBuffer>;
  type: string;
}) {
  const { t } = useI18n();
  const [src, setSrc] = useState<string | null>(null);
  const [state, setState] = useState<"idle" | "loading" | "failed">("idle");
  const [linkFailed, setLinkFailed] = useState(false);
  const stored = useStorageURL(props.throughStorage, linkFailed, props.type, null);
  const play = async () => {
    setState("loading");
    const url = await props.load().catch(() => null);
    if (url) setSrc(url);
    else if (props.throughStorage) setLinkFailed(true);
    else setState("failed");
  };
  const failed = state === "failed" || (linkFailed && stored.failed);
  if (failed) return <Unavailable browserURL={props.browserURL} />;
  const playing = linkFailed ? stored.url : src;
  const onError = () => (linkFailed || !props.throughStorage ? setState("failed") : setLinkFailed(true));
  if (playing) {
    return props.video ? (
      <video className="preview-media" src={playing} controls autoPlay onError={onError} />
    ) : (
      <audio className="preview-audio" src={playing} controls autoPlay onError={onError} />
    );
  }
  if (linkFailed) return <Spinner className="preview-play" size="medium" />;
  return (
    <>
      {props.poster ? <img className="preview-poster" src={props.poster} alt="" draggable={false} /> : <FileIcon filename={props.filename} size={44} />}
      {state === "loading" ? (
        <Spinner className="preview-play" size="medium" />
      ) : (
        <button type="button" className="preview-play" aria-label={t("Play")} title={t("Play")} onClick={play}>
          <PlayCircleFilled fontSize={56} />
        </button>
      )}
    </>
  );
}

function PdfFrame({ data }: { data: ArrayBuffer }) {
  const [src, setSrc] = useState<string | null>(null);
  useEffect(() => {
    const objectURL = URL.createObjectURL(new Blob([data], { type: "application/pdf" }));
    setSrc(objectURL);
    return () => URL.revokeObjectURL(objectURL);
  }, [data]);
  return src ? <iframe className="preview-pdf" src={src} title="PDF" /> : null;
}

export function Preview({ url, filename, mimeType, browserURL, placeholder, mediaURL, size, onZoom, throughStorage }: PreviewProps) {
  const { t } = useI18n();
  const kind = previewKind(filename, mimeType);
  const tooBig = size !== undefined && size > MAX_PREVIEW_BYTES;
  // Nothing past 25 MB is read through storage either.
  const fallback = tooBig ? undefined : throughStorage;
  const downloaded = (kind.kind === "pdf" || kind.kind === "text") && !tooBig;
  const file = useRemoteFile(url, downloaded, fallback);
  const [imageState, setImageState] = useState<"loading" | "ready" | "failed">("loading");
  /** The image's link didn't load (or there's none): read it through storage. */
  const [imageLinkFailed, setImageLinkFailed] = useState(false);
  const storedImage = useStorageURL(fallback, kind.kind === "image" && (imageLinkFailed || !url), blobType(filename, mimeType), url);

  useEffect(() => {
    setImageState("loading");
    setImageLinkFailed(false);
  }, [url]);
  const imageURL = imageLinkFailed || !url ? storedImage.url : url;
  const imageFailed = imageState === "failed" || ((imageLinkFailed || !url) && storedImage.failed);

  if (tooBig && (kind.kind === "pdf" || kind.kind === "text")) {
    return (
      <div className="preview preview-text">
        <Unavailable browserURL={browserURL} />
      </div>
    );
  }

  switch (kind.kind) {
    case "image":
      return (
        <button type="button" className="preview preview-image checkerboard" onClick={onZoom} disabled={!onZoom} title={filename}>
          {imageFailed ? (
            <Unavailable browserURL={browserURL} />
          ) : (
            <>
              {imageState === "loading" &&
                (placeholder ? <img className="preview-placeholder" src={placeholder} alt="" /> : <Spinner size="small" />)}
              {imageURL && (
                <img
                  src={imageURL}
                  alt=""
                  draggable={false}
                  style={imageState === "ready" ? undefined : { display: "none" }}
                  onLoad={() => setImageState("ready")}
                  onError={() => (fallback && !imageLinkFailed && url ? setImageLinkFailed(true) : setImageState("failed"))}
                />
              )}
            </>
          )}
        </button>
      );
    case "pdf":
      return (
        <div className="preview preview-tall">
          {file.state === "ready" ? (
            <PdfFrame data={file.data} />
          ) : file.state === "failed" ? (
            <Unavailable browserURL={browserURL} />
          ) : (
            <Spinner size="small" />
          )}
        </div>
      );
    case "text": {
      let text: string | null = null;
      if (file.state === "ready") {
        try {
          text = new TextDecoder("utf-8", { fatal: true }).decode(file.data);
        } catch {
          text = null;
        }
      }
      return (
        <div className="preview preview-text">
          {file.state === "loading" ? (
            <Spinner size="small" />
          ) : text === null ? (
            <Unavailable browserURL={browserURL} />
          ) : kind.markdown ? (
            <Markdown source={text} />
          ) : (
            <pre className="preview-plain">{text}</pre>
          )}
        </div>
      );
    }
    case "media":
      return (
        <div className="preview preview-player">
          <MediaPlayer
            key={url ?? filename}
            video={kind.video}
            filename={filename}
            poster={placeholder}
            load={mediaURL ?? (() => Promise.resolve(url))}
            browserURL={browserURL}
            throughStorage={fallback}
            type={blobType(filename, mimeType)}
          />
        </div>
      );
    default:
      return (
        <button type="button" className="preview preview-file" onClick={() => api.openUrl(browserURL)}>
          {placeholder ? (
            <img className="preview-thumbnail" src={placeholder} alt="" draggable={false} />
          ) : (
            <FileIcon filename={filename} size={44} />
          )}
          <Text size={200} className="link-text">
            {t("Open in Browser")}
          </Text>
        </button>
      );
  }
}
