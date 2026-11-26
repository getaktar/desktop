import { api, canDo, errorMessage, temporaryLinkDurations, type DestinationConfig, type UploadRecord } from "../lib/api";
import { temporaryLinkTitle } from "../lib/format";
import type { Translate } from "../lib/i18n";
import type { MenuEntry } from "./Dialogs";

/** "Copy Temporary Link" for an upload in history: a fresh presigned link,
 * which works even when the bucket is private or the one copied at upload
 * time has run out. Nothing for a destination that can't make them
 * (WebDAV) or was removed. */
export function temporaryLinkMenu(
  record: UploadRecord,
  destinations: DestinationConfig[],
  t: Translate,
  onError: (message: string) => void,
): MenuEntry[] {
  const destination = destinations.find((candidate) => candidate.id === record.destinationId);
  if (destination && !canDo(destination, "temporaryLinks")) return [];
  return [
    {
      submenu: t("Copy Temporary Link"),
      items: temporaryLinkDurations.map((seconds) => ({
        label: temporaryLinkTitle(seconds, t),
        onClick: () => {
          api
            .recordTemporaryLink(record.id, seconds)
            .then((link) => api.copyText(link))
            .catch((error) => onError(errorMessage(error)));
        },
      })),
    },
  ];
}
