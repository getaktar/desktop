import { Text } from "@fluentui/react-components";

import type { ConnectionResult, PublicLinkCheck, StorageKind } from "../lib/api";
import { useI18n, type Translate } from "../lib/i18n";

/** What Test Connection found, in the destination form and after an
 * import: why it couldn't reach the bucket at all (`error`), or one line
 * per step of the test, so a bucket that takes uploads but won't serve
 * them reads as a problem instead of a success. `kind` words the hints
 * for a folder on a WebDAV or SFTP server rather than a bucket. An SFTP
 * server's key is shown by its fingerprint, and a key that changed comes
 * first, with nothing else tried. */
export function ConnectionTestResult({
  result,
  error,
  kind = "s3",
}: {
  result: ConnectionResult | null;
  error: string | null;
  kind?: StorageKind;
}) {
  const { t } = useI18n();
  const s3 = kind === "s3";
  if (error) {
    return (
      <Text size={200} className="text-error">
        {error}
      </Text>
    );
  }
  if (!result) return null;
  if (result.previousHostKeyFingerprint) {
    return (
      <div className="test-result">
        <Text size={200} className="text-error">
          ✕ {t("The server’s identity has changed. Nothing was sent to it.")}
        </Text>
        <Text size={200} className="mono selectable break">
          {t("Saved key: {0}", result.previousHostKeyFingerprint)}
        </Text>
        <Text size={200} className="mono selectable break">
          {t("Key it shows now: {0}", result.hostKeyFingerprint ?? "?")}
        </Text>
        <Text size={200} className="secondary">
          {t("If you expected this (the server was set up again or moved), test again to sign in with the new key, then save to keep it. If not, don’t save: someone may be in between.")}
        </Text>
      </div>
    );
  }
  const link = result.publicLink;
  const hint = s3 ? publicLinkHint(link, t) : folderLinkHint(link, t);
  return (
    <div className="test-result">
      {result.hostKeyFingerprint && (
        <Text size={200} className="mono selectable break">
          {t("Server key: {0}", result.hostKeyFingerprint)}
        </Text>
      )}
      {result.writable ? (
        <Text size={200} className="text-success">
          ✓ {t("Upload: works")}
        </Text>
      ) : (
        <Text size={200} className="text-error">
          ✕ {s3 ? t("Upload: failed. This key can read the bucket but can’t write to it.") : t("Upload: failed. This account can read the folder but can’t write to it.")}
        </Text>
      )}
      {link?.kind === "reachable" && (
        <Text size={200} className="text-success">
          ✓ {t("Public link: works")}
        </Text>
      )}
      {link?.kind === "status" && (
        <Text size={200} className="text-error">
          ✕ {t("Public link: failed (HTTP {0})", String(link.code))}
        </Text>
      )}
      {link?.kind === "noResponse" && (
        <Text size={200} className="text-error">
          ✕ {t("Public link: no response")}
        </Text>
      )}
      {hint && (
        <Text size={200} className="secondary">
          {hint}
        </Text>
      )}
    </div>
  );
}

function publicLinkHint(check: PublicLinkCheck | null, t: Translate) {
  if (!check || check.kind === "reachable") return null;
  if (check.kind === "status" && (check.code === 401 || check.code === 403)) {
    return t(
      "Uploads work, but anyone who opens a link gets an error. Allow public reads on the bucket (on R2, turn on the r2.dev URL or connect a custom domain), or keep it private and share files with Copy Temporary Link in the Library.",
    );
  }
  if (check.kind === "status" && check.code === 404) {
    return t("The test file was uploaded, but it isn’t at the Public Base URL. Check that the URL points to this bucket.");
  }
  return t("The Public Base URL didn’t serve the test file. Check the domain and that it points to this bucket.");
}

/** `publicLinkHint` for a WebDAV folder, which has no bucket policy or
 * temporary links: its public address has to serve it by itself. */
function folderLinkHint(check: PublicLinkCheck | null, t: Translate) {
  if (!check || check.kind === "reachable") return null;
  if (check.kind === "status" && (check.code === 401 || check.code === 403)) {
    return t("Uploads work, but anyone who opens a link gets an error. The Public Base URL has to serve this folder without signing in.");
  }
  if (check.kind === "status" && check.code === 404) {
    return t("The test file was uploaded, but it isn’t at the Public Base URL. Check that the URL points to this folder.");
  }
  return t("The Public Base URL didn’t serve the test file. Check the domain and that it points to this folder.");
}
